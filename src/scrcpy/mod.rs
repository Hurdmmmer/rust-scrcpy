//! Scrcpy 领域根模块。
//!
//! 设计说明：
//! - 本模块用于承接新架构迁移（client/config/session/runtime）；
//! - 当前目录为 scrcpy 业务主实现，不依赖旧核心目录。
//! - 在完成全量迁移前，不替换现有业务调用路径。

pub mod client;
pub mod config;
/// 新架构解码内核。
pub mod decode_core;
pub mod runtime;
pub mod session;
pub mod scrcpy_service;






