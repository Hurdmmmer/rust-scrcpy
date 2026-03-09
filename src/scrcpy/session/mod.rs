//! Scrcpy 单会话模块。
//!
//! 设计边界：
//! - `Session` 只承载“单条 scrcpy 会话”的底层资源；
//! - 负责建立与释放 video/control 通道，并对外提供基础控制能力；
//! - 不承担项目级编排、事件分发、解码调度与会话表管理。

pub mod session;
pub mod session_manager;

pub use session::Session;
pub use session_manager::SessionManager;
