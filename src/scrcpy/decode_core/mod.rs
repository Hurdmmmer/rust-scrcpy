//! Scrcpy 解码内核模块。
//!
//! 说明：
//! - 本目录承载新架构解码核心实现；
//! - 迁移原则是“路径与命名重构，行为保持不变”；
//! - 对上层统一暴露解码器、解码流水线、GPU 上传能力。

pub mod backend;
pub mod decode_pipeline;
pub mod ffmpeg_decoder;
pub mod gpu_direct_output;

pub use backend::{
    DecodedFrame, DecoderFactory, DecoderOutputMode, DecoderPreference, DecoderStats, VideoDecoder,
};
pub use decode_pipeline::{DecoderPipeline, PipelineConfig, PipelineEvent, PipelineStats};
pub use ffmpeg_decoder::{BgraFrame, FfmpegDecoder};
pub use gpu_direct_output::{D3D11Context, D3D11TextureUploader};






