use crate::gh_common::{Result, ScrcpyError};
use tracing::{debug, info, warn};
#[cfg(target_os = "windows")]
use windows::core::Interface;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Dxgi::IDXGIResource;

/// 硬解调试常量：指定具体硬解名称（`None` 表示自动探测）。
/// 是否禁用 `h264_cuvid`（默认不禁用）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfmpegDecoderMode {
    PreferHw,
    ForceHw,
    ForceSw,
}

impl FfmpegDecoderMode {
    fn from_env() -> Self {
        Self::PreferHw
    }
}

/// 解码后输出给渲染链路的 BGRA 帧。
#[derive(Clone, Debug)]
pub struct BgraFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts: i64,
}

/// 解码器输出帧抽象。
///
/// 设计目的：
/// - 为“解码直接输出 GPU 资源”预留统一出口；
/// - 现阶段保持与历史行为一致，默认仍输出 `CpuBgra`；
/// - 后续接入 D3D11VA 直出时，仅需新增 `GpuShared` 生产逻辑，backend 无需再改签名。
#[derive(Clone, Debug)]
pub enum FfmpegDecodedFrame {
    /// CPU BGRA 像素帧（当前主路径）。
    CpuBgra(BgraFrame),
    /// GPU 共享纹理句柄帧。
    GpuShared {
        handle: i64,
        width: u32,
        height: u32,
        pts: i64,
    },
}

/// FFmpeg H.264 解码器（可选硬解，失败自动回落软解）。
///
/// 设计目标：
/// - 尽量复用硬件解码能力；
/// - 在旋转/分辨率变化时可重建像素转换器；
/// - 统一输出 BGRA，便于 D3D11 纹理上传。
pub struct FfmpegDecoder {
    decoder: ffmpeg_next::decoder::Video,
    scaler: Option<ffmpeg_next::software::scaling::Context>,
    scaler_src_width: u32,
    scaler_src_height: u32,
    scaler_src_format: Option<ffmpeg_next::format::Pixel>,
    frame_count: u64,
    packet_buf: Vec<u8>,
    /// 是否已经打印过“硬件帧直出失败”告警，避免每帧刷屏。
    direct_gpu_export_warned: bool,
}

impl FfmpegDecoder {
    #[inline]
    fn has_start_code(data: &[u8]) -> bool {
        (data.len() >= 4 && data[0] == 0 && data[1] == 0 && data[2] == 0 && data[3] == 1)
            || (data.len() >= 3 && data[0] == 0 && data[1] == 0 && data[2] == 1)
    }

    /// 创建解码器实例：
    /// 1. 初始化 FFmpeg；
    /// 2. 按优先级探测硬解；
    /// 3. 若不可用则回退到软件解码。
    pub fn new() -> Result<Self> {
        Self::new_with_mode(FfmpegDecoderMode::from_env())
    }

    /// 按指定模式创建解码器。
    ///
    /// 说明：
    /// - `PreferHw`：优先硬解，不可用则自动软解；
    /// - `ForceHw`：仅硬解；
    /// - `ForceSw`：仅软解。
    pub fn new_with_mode(mode: FfmpegDecoderMode) -> Result<Self> {
        ffmpeg_next::init()
            .map_err(|e| ScrcpyError::Decode(format!("FFmpeg init failed: {}", e)))?;
        info!("FFmpeg initialized");
        info!("Decoder mode: {:?}", mode);

        let requested_hw_decoder: Option<String> = None;

        // 按优先级探测可用硬解。注意：这里只能说明“可被发现”，
        // 运行时仍可能因驱动/上下文问题失败。
        let hw_decoders = vec![
            ("h264_nvdec", "NVIDIA NVDEC"),
            ("h264_cuvid", "NVIDIA CUVID"),
            ("h264_d3d11va", "DirectX D3D11VA"),
            ("h264_qsv", "Intel Quick Sync"),
        ];

        let mut codec = None;
        let mut decoder_name = String::new();

        if mode != FfmpegDecoderMode::ForceSw {
            if let Some(ref force_name) = requested_hw_decoder {
                if let Some(hw_codec) =
                    ffmpeg_next::codec::decoder::find_by_name(force_name.as_str())
                {
                    info!("Found requested hw decoder: {}", force_name);
                    codec = Some(hw_codec);
                    decoder_name = force_name.clone();
                } else {
                    warn!("Requested hw decoder not found: {}", force_name);
                }
            }

            if codec.is_none() {
                for (hw_name, hw_desc) in &hw_decoders {
                    if let Some(hw_codec) = ffmpeg_next::codec::decoder::find_by_name(hw_name) {
                        info!("Found hw decoder: {} ({})", hw_name, hw_desc);
                        codec = Some(hw_codec);
                        decoder_name = hw_name.to_string();
                        break;
                    } else {
                        info!("Hw decoder not available: {} ({})", hw_name, hw_desc);
                    }
                }
            }
        }

        let (final_codec, is_hardware) = if let Some(hw_codec) = codec {
            (hw_codec, true)
        } else if mode == FfmpegDecoderMode::ForceHw {
            return Err(ScrcpyError::Decode(
                "force_hw mode enabled, but no hardware decoder found".to_string(),
            ));
        } else {
            info!("No hw decoder found, fallback to software");
            let sw_codec = ffmpeg_next::codec::decoder::find(ffmpeg_next::codec::Id::H264)
                .ok_or_else(|| ScrcpyError::Decode("H264 decoder not found".to_string()))?;
            decoder_name = "h264 (software)".to_string();
            (sw_codec, false)
        };

        if is_hardware {
            info!("Using hw decoder: {}", decoder_name);
        } else {
            warn!("Using software decoder: {}", decoder_name);
        }

        let context = ffmpeg_next::codec::context::Context::new_with_codec(final_codec);
        let mut video_context = context
            .decoder()
            .video()
            .map_err(|e| ScrcpyError::Decode(format!("create decoder failed: {}", e)))?;

        // 低延迟标记：尽量减少编解码缓存带来的实时性损失。
        unsafe {
            (*video_context.as_mut_ptr()).flags |= ffmpeg_next::sys::AV_CODEC_FLAG_LOW_DELAY as i32;
        }

        // 软件解码开启多线程，缓解 CPU 压力。
        if !is_hardware {
            video_context.set_threading(ffmpeg_next::codec::threading::Config {
                kind: ffmpeg_next::codec::threading::Type::Frame,
                count: 0,
            });
        }

        Ok(Self {
            decoder: video_context,
            scaler: None,
            scaler_src_width: 0,
            scaler_src_height: 0,
            scaler_src_format: None,
            frame_count: 0,
            packet_buf: Vec::with_capacity(1024 * 1024),
            direct_gpu_export_warned: false,
        })
    }

    /// 尝试从硬件帧中直接提取 D3D11 共享句柄（Windows）。
    ///
    /// 说明：
    /// - 仅当 FFmpeg 输出为 D3D11 硬件像素格式时尝试；
    /// - 成功则可直接走 `GpuShared`，跳过 `BGRA -> upload`；
    /// - 失败时回退到 CPU BGRA 路径，保证功能可用。
    #[cfg(target_os = "windows")]
    fn try_extract_d3d11_shared_handle(
        &mut self,
        frame: &ffmpeg_next::util::frame::Video,
    ) -> Option<(i64, u32, u32, i64)> {
        use ffmpeg_next::format::Pixel;
        let fmt = frame.format();
        if fmt != Pixel::D3D11VA_VLD && fmt != Pixel::D3D11 {
            return None;
        }

        let (width, height, pts) = (frame.width(), frame.height(), frame.pts().unwrap_or(0));
        unsafe {
            let av = frame.as_ptr();
            let raw_texture_ptr = (*av).data[0] as *mut core::ffi::c_void;
            if raw_texture_ptr.is_null() {
                return None;
            }

            let tex = ID3D11Texture2D::from_raw_borrowed(&raw_texture_ptr)?;
            let dxgi_res: IDXGIResource = match tex.cast() {
                Ok(v) => v,
                Err(_) => return None,
            };
            let handle = match dxgi_res.GetSharedHandle() {
                Ok(v) => v,
                Err(_) => return None,
            };
            Some((handle.0 as i64, width, height, pts))
        }
    }

    /// 非 Windows 平台占位实现：始终不走 D3D11 直出。
    #[cfg(not(target_os = "windows"))]
    fn try_extract_d3d11_shared_handle(
        &mut self,
        _frame: &ffmpeg_next::util::frame::Video,
    ) -> Option<(i64, u32, u32, i64)> {
        None
    }

    /// 解码一个 Annex-B 包。
    ///
    /// 输入可为：
    /// 1) 单个不带起始码的 NAL；
    /// 2) 已带起始码的 Annex-B 字节流（可包含多个 NAL）。
    ///
    /// 返回该包产出的 0..N 帧（支持 CPU/GPU 双形态）。
    pub fn decode(&mut self, packet_data: &[u8]) -> Result<Vec<FfmpegDecodedFrame>> {
        if packet_data.is_empty() {
            return Ok(Vec::new());
        }

        self.packet_buf.clear();
        // 对齐 scrcpy demuxer 语义：优先按“原始 packet”送解码器，
        // 避免客户端二次猜测 NAL/AU 边界导致包语义被破坏。
        self.packet_buf.extend_from_slice(packet_data);
        let mut send_ok = false;
        let mut send_err_msg = String::new();
        {
            let packet = ffmpeg_next::codec::packet::Packet::copy(&self.packet_buf);
            match self.decoder.send_packet(&packet) {
                Ok(_) => send_ok = true,
                Err(e) => send_err_msg = e.to_string(),
            }
        }

        if !send_ok {
            // 兼容旧 raw 单 NAL 输入：仅在“原样送包失败”且输入不含起始码时，
            // 才退回补 0x00000001 重试一次。
            let mut retry_ok = false;
            if !Self::has_start_code(packet_data) {
                self.packet_buf.clear();
                self.packet_buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                self.packet_buf.extend_from_slice(packet_data);
                let retry_packet = ffmpeg_next::codec::packet::Packet::copy(&self.packet_buf);
                match self.decoder.send_packet(&retry_packet) {
                    Ok(_) => {
                        retry_ok = true;
                        debug!("decode send_packet fallback success: prefixed Annex-B");
                    }
                    Err(retry_err) => {
                        send_err_msg = format!("{}; retry={}", send_err_msg, retry_err);
                    }
                }
            }

            if !retry_ok {
                if send_err_msg.contains("Invalid data") {
                    // 配置包或解码器缓存阶段常见，不视为致命。
                    debug!("config/invalid packet cached by decoder");
                    return Ok(Vec::new());
                }
                return Err(ScrcpyError::Decode(format!(
                    "send packet failed: {}",
                    send_err_msg
                )));
            }
        }

        // 对 VCL NAL 执行“全量拉帧”：
        // 一个 send_packet 可能产出 0..N 帧，这里循环 drain 到 EAGAIN。
        let mut outputs = Vec::new();
        loop {
            let mut decoded_frame = ffmpeg_next::util::frame::Video::empty();
            match self.decoder.receive_frame(&mut decoded_frame) {
                Ok(_) => {
                    self.frame_count += 1;
                    if let Some((handle, width, height, pts)) =
                        self.try_extract_d3d11_shared_handle(&decoded_frame)
                    {
                        debug!(
                            "decode direct gpu frame: format={:?}, handle={}, {}x{}, pts={}",
                            decoded_frame.format(),
                            handle,
                            width,
                            height,
                            pts
                        );
                        outputs.push(FfmpegDecodedFrame::GpuShared {
                            handle,
                            width,
                            height,
                            pts,
                        });
                        continue;   
                    }

                    if matches!(
                        decoded_frame.format(),
                        ffmpeg_next::format::Pixel::D3D11VA_VLD | ffmpeg_next::format::Pixel::D3D11
                    ) && !self.direct_gpu_export_warned
                    {
                        self.direct_gpu_export_warned = true;
                        warn!(
                            "hardware frame direct export unavailable, fallback to CPU BGRA path"
                        );
                    }

                    let width = decoded_frame.width();
                    let height = decoded_frame.height();
                    let pts = decoded_frame.pts().unwrap_or(0);
                    let bgra_data = self.convert_to_bgra(&decoded_frame)?;
                    outputs.push(FfmpegDecodedFrame::CpuBgra(BgraFrame {
                        data: bgra_data,
                        width,
                        height,
                        pts,
                    }));
                }
                // EAGAIN：当前包可取的帧已经取完。
                Err(ffmpeg_next::Error::Other { errno: 11 }) => break,
                Err(e) => {
                    warn!("receive frame failed: {:?}", e);
                    break;
                }
            }
        }
        Ok(outputs)
    }

    /// 将解码帧转换为 BGRA。
    ///
    /// 当输入分辨率或像素格式变化（常见于旋转切换）时重建 scaler。
    fn convert_to_bgra(&mut self, frame: &ffmpeg_next::util::frame::Video) -> Result<Vec<u8>> {
        let width = frame.width();
        let height = frame.height();
        let format = frame.format();

        let need_recreate = self.scaler.is_none()
            || self.scaler_src_width != width
            || self.scaler_src_height != height
            || self.scaler_src_format != Some(format);

        if need_recreate {
            let scaler = ffmpeg_next::software::scaling::Context::get(
                format,
                width,
                height,
                ffmpeg_next::format::Pixel::BGRA,
                width,
                height,
                ffmpeg_next::software::scaling::Flags::BILINEAR,
            )
            .map_err(|e| ScrcpyError::Decode(format!("create scaler failed: {}", e)))?;
            self.scaler = Some(scaler);
            self.scaler_src_width = width;
            self.scaler_src_height = height;
            self.scaler_src_format = Some(format);
        }

        let scaler = self.scaler.as_mut().unwrap();
        let mut bgra_frame = ffmpeg_next::util::frame::Video::empty();
        scaler
            .run(frame, &mut bgra_frame)
            .map_err(|e| ScrcpyError::Decode(format!("scale failed: {}", e)))?;

        let bgra_data_size = (width * height * 4) as usize;
        let mut bgra_data = Vec::with_capacity(bgra_data_size);

        // 按行拷贝，处理 stride 可能大于 width*4 的情况。
        unsafe {
            let data_ptr = bgra_frame.data(0).as_ptr();
            let linesize = bgra_frame.stride(0);
            for y in 0..height as usize {
                let row_ptr = data_ptr.add(y * linesize);
                let row_slice = std::slice::from_raw_parts(row_ptr, (width * 4) as usize);
                bgra_data.extend_from_slice(row_slice);
            }
        }

        Ok(bgra_data)
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        info!(
            "FFmpeg decoder dropped, decoded {} frames",
            self.frame_count
        );
    }
}
