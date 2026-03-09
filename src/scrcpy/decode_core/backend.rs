use crate::scrcpy::decode_core::ffmpeg_decoder::{BgraFrame, FfmpegDecoder, FfmpegDecoderMode};
use crate::scrcpy::decode_core::gpu_direct_output::{D3D11Context, D3D11TextureUploader};
use crate::gh_common::{Result, ScrcpyError};
use tracing::{debug, info, warn};

/// 对外统一的解码输出。
#[derive(Clone, Debug)]
pub enum DecodedFrame {
    /// CPU 内存中的 BGRA 帧（软解、硬解统一可落地输出）。
    CpuBgra(BgraFrame),
    /// 预留：真实 GPU 共享纹理句柄输出。
    GpuShared {
        handle: i64,
        width: u32,
        height: u32,
        pts: i64,
    },
}

/// 解码器统计信息。
#[derive(Clone, Debug, Default)]
pub struct DecoderStats {
    pub decoded_frames: u64,
    pub dropped_frames: u64,
}

/// 解码器策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderPreference {
    PreferHardware,
    ForceHardware,
    ForceSoftware,
}

/// 解码输出模式：
/// - `GpuShared`：维持现有 DXGI 共享纹理句柄链路（默认）；
/// - `CpuBgra`：输出 CPU BGRA，用于 PixelBuffer V2 纯内存渲染链路。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderOutputMode {
    GpuShared,
    CpuBgra,
}

impl DecoderPreference {
    pub fn from_env() -> Self {
        // 现阶段由代码配置主导，默认“优先硬解”。
        Self::PreferHardware
    }
}

/// 统一解码接口。
pub trait VideoDecoder {
    fn name(&self) -> &'static str;
    fn decode(&mut self, packet: &[u8]) -> Result<Vec<DecodedFrame>>;
    fn flush(&mut self) -> Result<Vec<DecodedFrame>>;
    fn stats(&self) -> DecoderStats;
}

/// 软件解码实现。
pub struct SoftwareDecoder {
    inner: FfmpegDecoder,
    uploader: Option<D3D11TextureUploader>,
    output_mode: DecoderOutputMode,
    stats: DecoderStats,
}

impl SoftwareDecoder {
    pub fn new(output_mode: DecoderOutputMode) -> Result<Self> {
        let inner = FfmpegDecoder::new_with_mode(FfmpegDecoderMode::ForceSw)?;
        let uploader = match output_mode {
            DecoderOutputMode::GpuShared => {
                let ctx = D3D11Context::new().map_err(|e| {
                    crate::gh_common::ScrcpyError::Decode(format!(
                        "create d3d11 context failed: {}",
                        e
                    ))
                })?;
                Some(D3D11TextureUploader::new_with_context(&ctx).map_err(|e| {
                    crate::gh_common::ScrcpyError::Decode(format!(
                        "create texture uploader failed: {}",
                        e
                    ))
                })?)
            }
            DecoderOutputMode::CpuBgra => None,
        };
        Ok(Self {
            inner,
            uploader,
            output_mode,
            stats: DecoderStats::default(),
        })
    }
}

impl VideoDecoder for SoftwareDecoder {
    fn name(&self) -> &'static str {
        "ffmpeg-sw"
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Vec<DecodedFrame>> {
        debug!("software decode input bytes={}", packet.len());
        let frames = self.inner.decode(packet)?;
        self.stats.decoded_frames = self.stats.decoded_frames.saturating_add(frames.len() as u64);
        debug!("software decode output frames={}", frames.len());
        let mut out = Vec::with_capacity(frames.len());
        for frame in frames {
            match self.output_mode {
                DecoderOutputMode::GpuShared => {
                    let uploader = self.uploader.as_mut().ok_or_else(|| {
                        ScrcpyError::Decode("software uploader unavailable".to_string())
                    })?;
                    let handle = uploader
                        .upload_bgra_frame(frame.width, frame.height, frame.pts, &frame.data)
                        .map_err(|e| {
                            ScrcpyError::Decode(format!("software uploader failed: {}", e))
                        })?;
                    out.push(DecodedFrame::GpuShared {
                        handle: handle as i64,
                        width: frame.width,
                        height: frame.height,
                        pts: frame.pts,
                    });
                }
                DecoderOutputMode::CpuBgra => out.push(DecodedFrame::CpuBgra(frame)),
            }
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        Ok(Vec::new())
    }

    fn stats(&self) -> DecoderStats {
        self.stats.clone()
    }
}

/// 硬件解码实现（生产可用）：
/// - 解码路径优先使用硬件；
/// - 输出统一为 CpuBgra，便于现有渲染链路稳定落地。
pub struct HardwareDecoderStub {
    inner: FfmpegDecoder,
    uploader: Option<D3D11TextureUploader>,
    output_mode: DecoderOutputMode,
    stats: DecoderStats,
}

impl HardwareDecoderStub {
    pub fn new(output_mode: DecoderOutputMode) -> Result<Self> {
        let inner = FfmpegDecoder::new_with_mode(FfmpegDecoderMode::ForceHw)?;
        let uploader = match output_mode {
            DecoderOutputMode::GpuShared => {
                let ctx = D3D11Context::new().map_err(|e| {
                    crate::gh_common::ScrcpyError::Decode(format!(
                        "create d3d11 context failed: {}",
                        e
                    ))
                })?;
                Some(D3D11TextureUploader::new_with_context(&ctx).map_err(|e| {
                    crate::gh_common::ScrcpyError::Decode(format!(
                        "create texture uploader failed: {}",
                        e
                    ))
                })?)
            }
            DecoderOutputMode::CpuBgra => None,
        };
        info!("hardware decoder initialized, output_mode={:?}", output_mode);
        Ok(Self {
            inner,
            uploader,
            output_mode,
            stats: DecoderStats::default(),
        })
    }
}

impl VideoDecoder for HardwareDecoderStub {
    fn name(&self) -> &'static str {
        "ffmpeg-hw"
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Vec<DecodedFrame>> {
        debug!("hardware decode input bytes={}", packet.len());
        let frames = self.inner.decode(packet)?;
        self.stats.decoded_frames = self.stats.decoded_frames.saturating_add(frames.len() as u64);
        debug!("hardware decode output frames={}", frames.len());
        let mut out = Vec::with_capacity(frames.len());
        for frame in frames {
            match self.output_mode {
                DecoderOutputMode::GpuShared => {
                    let uploader = self.uploader.as_mut().ok_or_else(|| {
                        ScrcpyError::Decode("hardware uploader unavailable".to_string())
                    })?;
                    let handle = uploader
                        .upload_bgra_frame(frame.width, frame.height, frame.pts, &frame.data)
                        .map_err(|e| {
                            ScrcpyError::Decode(format!("hardware uploader failed: {}", e))
                        })?;
                    out.push(DecodedFrame::GpuShared {
                        handle: handle as i64,
                        width: frame.width,
                        height: frame.height,
                        pts: frame.pts,
                    });
                }
                DecoderOutputMode::CpuBgra => out.push(DecodedFrame::CpuBgra(frame)),
            }
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        Ok(Vec::new())
    }

    fn stats(&self) -> DecoderStats {
        self.stats.clone()
    }
}

/// 解码器工厂：统一入口 + 自动降级。
pub struct DecoderFactory;

impl DecoderFactory {
    pub fn create_from_env() -> Result<Box<dyn VideoDecoder>> {
        Self::create(DecoderPreference::from_env(), DecoderOutputMode::GpuShared)
    }

    pub fn create(
        preference: DecoderPreference,
        output_mode: DecoderOutputMode,
    ) -> Result<Box<dyn VideoDecoder>> {
        match preference {
            DecoderPreference::ForceSoftware => {
                info!("decoder factory: force software");
                let decoder: Box<dyn VideoDecoder> = Box::new(SoftwareDecoder::new(output_mode)?);
                info!("decoder selected: {}", decoder.name());
                Ok(decoder)
            }
            DecoderPreference::ForceHardware => {
                info!("decoder factory: force hardware");
                let decoder: Box<dyn VideoDecoder> =
                    Box::new(HardwareDecoderStub::new(output_mode)?);
                info!("decoder selected: {}", decoder.name());
                Ok(decoder)
            }
            DecoderPreference::PreferHardware => {
                info!("decoder factory: prefer hardware");
                match HardwareDecoderStub::new(output_mode) {
                    Ok(hw) => {
                        let decoder: Box<dyn VideoDecoder> = Box::new(hw);
                        info!("decoder selected: {}", decoder.name());
                        Ok(decoder)
                    }
                    Err(e) => {
                        warn!("hardware decoder unavailable: {}, fallback to software", e);
                        let decoder: Box<dyn VideoDecoder> =
                            Box::new(SoftwareDecoder::new(output_mode)?);
                        info!("decoder selected: {}", decoder.name());
                        Ok(decoder)
                    }
                }
            }
        }
    }
}






