//! Scrcpy 运行时模块。
//!
//! 设计边界：
//! - `ScrcpyCoreRuntime` 只负责会话生命周期编排与路由；
//! - `scrcpy_decode_pipeline` 负责解码链路与恢复策略；
//! - 底层连接由 `ScrcpyClient` 负责，单会话能力由 `Session` 负责；
//! - 不引入旧架构的兼容入口。

pub mod scrcpy_core_runtime;
pub mod scrcpy_decode_pipeline;

pub use scrcpy_core_runtime::ScrcpyCoreRuntime;
pub use scrcpy_decode_pipeline::{DecodeFrame, ScrcpyDecodeConfig, ScrcpyDecodeEvent, ScrcpyDecodePipeline};
