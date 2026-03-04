//! 对外稳定 API 门面（Facade）。
//!
//! 设计约束：
//! - 本文件只负责“对外签名与注释”，类似 C/C++ 头文件；
//! - 业务编排放在 `service`，会话执行放在 `runtime`；
//! - FRB 入口固定为 `crate::rust_scrcpy_api`，避免内部重构影响上层调用名。

pub(crate) mod model;
pub(crate) mod runtime;
pub(crate) mod service;

pub use model::{
    DecoderMode, DeviceInfo, ErrorCode, LogLevel, OrientationChangeSource, OrientationMode,
    RenderPipelineMode, SessionConfig, SessionConfigV2, SessionEvent, SessionStats, SystemKey,
    TextureFrame,
};

use crate::error::Result;
use crate::scrcpy::control::{KeyEvent, ScrollEvent, TouchEvent};

/// 初始化 Rust 侧日志系统。
///
/// 语义：
/// - 仅首次调用生效，后续调用幂等返回；
/// - 推荐在应用启动阶段调用一次。
pub async fn setup_logger(max_level: LogLevel) -> Result<()> {
    service::setup_logger(max_level).await
}

/// 列出 ADB 在线设备。
///
/// 返回值中的 `device_id` 可直接用于创建会话。
pub async fn list_devices(adb_path: String) -> Result<Vec<DeviceInfo>> {
    service::list_devices(adb_path).await
}

/// 查询单个设备详情（型号、系统版本、分辨率等）。
pub async fn get_device_info(adb_path: String, device_id: String) -> Result<DeviceInfo> {
    service::get_device_info(adb_path, device_id).await
}

/// 创建会话（仅注册，不启动）。
///
/// 调用方需继续调用 [`start_session`] 进入运行态。
pub async fn create_session(config: SessionConfig) -> Result<String> {
    service::create_session(config).await
}

/// 创建会话（V2：支持渲染模式与解码模式配置）。
pub async fn create_session_v2(config: SessionConfigV2) -> Result<String> {
    service::create_session_v2(config).await
}

/// 启动指定会话。
///
/// 幂等：重复启动同一运行中会话会被忽略。
pub async fn start_session(session_id: String) -> Result<()> {
    service::start_session(session_id).await
}

/// 停止指定会话。
///
/// 停止后会话仍保留，可再次 `start_session`。
pub async fn stop_session(session_id: String) -> Result<()> {
    service::stop_session(session_id).await
}

/// 销毁会话并移除会话状态。
///
/// 销毁后该 `session_id` 不可复用。
pub async fn dispose_session(session_id: String) -> Result<()> {
    service::dispose_session(session_id).await
}


/// 发送触摸事件到设备。
pub async fn send_touch(session_id: String, event: TouchEvent) -> Result<()> {
    service::send_touch(session_id, event).await
}

/// 发送按键事件到设备。
pub async fn send_key(session_id: String, event: KeyEvent) -> Result<()> {
    service::send_key(session_id, event).await
}

/// 发送滚轮事件到设备。
pub async fn send_scroll(session_id: String, event: ScrollEvent) -> Result<()> {
    service::send_scroll(session_id, event).await
}

/// 发送文本输入到设备。
pub async fn send_text(session_id: String, text: String) -> Result<()> {
    service::send_text(session_id, text).await
}

/// 发送系统语义按键（Home/Back/音量等）。
pub async fn send_system_key(session_id: String, key: SystemKey) -> Result<()> {
    service::send_system_key(session_id, key).await
}

/// 设置设备剪贴板。
///
/// `paste=true` 时会触发设备端粘贴动作（由 scrcpy server 能力决定）。
pub async fn set_clipboard(session_id: String, text: String, paste: bool) -> Result<()> {
    service::set_clipboard(session_id, text, paste).await
}

/// 设置会话方向模式。
///
/// 该接口表达“方向意图”，实际生效以 `ResolutionChanged` 事件为准。
pub async fn set_orientation_mode(session_id: String, mode: OrientationMode) -> Result<()> {
    service::set_orientation_mode(session_id, mode).await
}

/// 请求关键帧（IDR），用于重同步加速恢复。
pub async fn request_idr(session_id: String) -> Result<()> {
    service::request_idr(session_id).await
}

/// 查询会话统计快照（FPS、时延、累计帧计数等）。
pub async fn get_session_stats(session_id: String) -> Result<SessionStats> {
    service::get_session_stats(session_id).await
}
