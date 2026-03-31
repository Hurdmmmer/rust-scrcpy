//! Flutter 对外 API。
//!
//! 设计约束：
//! - 本文件只保留 Flutter 侧需要调用的顶层函数；
//! - 会话服务与运行时实现下沉到 `scrcpy::scrcpy_service`；
//! - 回调注册由 `flutter_callback_register` 独立承载。

pub use crate::gh_common::model::{
    DecoderMode, DeviceInfo, LogEvent, LogLevel, OrientationChangeSource, OrientationMode,
    RenderPipelineMode, SessionConfig, SessionConfigV2, SessionEvent, SessionStats, SystemKey,
    TextureFrame, YoloConfig, YoloExecutionProvider, YoloFrameResult,
};

use crate::frb_generated::StreamSink;
use crate::gh_common::model::{KeyEvent, ScrollEvent, TouchEvent};
use crate::gh_common::Result;
use crate::scrcpy::scrcpy_service as session_service;

/// 初始化 Rust 侧日志系统。
///
/// 语义：
/// - 仅首次调用生效，后续调用幂等返回；
/// - 推荐在应用启动阶段调用一次。
pub async fn setup_logger(max_level: LogLevel) -> Result<()> {
    session_service::setup_logger(max_level).await
}

/// 列出 ADB 在线设备。
///
/// 返回值中的 `device_id` 可直接用于创建会话。
pub async fn list_devices(adb_path: String) -> Result<Vec<DeviceInfo>> {
    session_service::list_devices(adb_path).await
}

/// 查询单个设备详情（型号、系统版本、分辨率等）。
pub async fn get_device_info(adb_path: String, device_id: String) -> Result<DeviceInfo> {
    session_service::get_device_info(adb_path, device_id).await
}

/// 创建会话（仅注册，不启动）。
///
/// 调用方需继续调用 [`start_session`] 进入运行态。
pub async fn create_session(config: SessionConfig) -> Result<String> {
    session_service::create_session(config).await
}

/// 创建会话（V2：支持渲染模式与解码模式配置）。
pub async fn create_session_v2(config: SessionConfigV2) -> Result<String> {
    session_service::create_session_v2(config).await
}

/// 启动指定会话。
///
/// 幂等：重复启动同一运行中会话会被忽略。
pub async fn start_session(session_id: String) -> Result<()> {
    session_service::start_session(session_id).await
}

/// 停止指定会话。
///
/// 停止后会话仍保留，可再次 `start_session`。
pub async fn stop_session(session_id: String) -> Result<()> {
    session_service::stop_session(session_id).await
}

/// 销毁会话并移除会话状态。
///
/// 销毁后该 `session_id` 不可复用。
pub async fn dispose_session(session_id: String) -> Result<()> {
    session_service::dispose_session(session_id).await
}

/// 发送触摸事件到设备。
pub async fn send_touch(session_id: String, event: TouchEvent) -> Result<()> {
    session_service::send_touch(session_id, event).await
}

/// 发送按键事件到设备。
pub async fn send_key(session_id: String, event: KeyEvent) -> Result<()> {
    session_service::send_key(session_id, event).await
}

/// 发送滚轮事件到设备。
pub async fn send_scroll(session_id: String, event: ScrollEvent) -> Result<()> {
    session_service::send_scroll(session_id, event).await
}

/// 发送文本输入到设备。
pub async fn send_text(session_id: String, text: String) -> Result<()> {
    session_service::send_text(session_id, text).await
}

/// 发送系统语义按键（Home/Back/音量等）。
pub async fn send_system_key(session_id: String, key: SystemKey) -> Result<()> {
    session_service::send_system_key(session_id, key).await
}

/// 设置设备剪贴板。
///
/// `paste=true` 时会触发设备端粘贴动作（由 scrcpy server 能力决定）。
pub async fn set_clipboard(session_id: String, text: String, paste: bool) -> Result<()> {
    session_service::set_clipboard(session_id, text, paste).await
}

/// 设置会话方向模式。
///
/// 该接口表达“方向意图”，实际生效以 `ResolutionChanged` 事件为准。
pub async fn set_orientation_mode(session_id: String, mode: OrientationMode) -> Result<()> {
    session_service::set_orientation_mode(session_id, mode).await
}

/// 请求关键帧（IDR），用于重同步加速恢复。
pub async fn request_idr(session_id: String) -> Result<()> {
    session_service::request_idr(session_id).await
}

/// 查询会话统计快照（FPS、时延、累计帧计数等）。
pub async fn get_session_stats(session_id: String) -> Result<SessionStats> {
    session_service::get_session_stats(session_id).await
}

/// 订阅会话事件流（替代 Runner -> MethodChannel 事件桥）。
///
/// 说明：
/// - 该接口返回后，Rust 将异步持续向 `sink` 推送 `SessionEvent`；
/// - 调用方取消 Dart 订阅后，Rust 转发线程会自动退出。
pub async fn subscribe_session_events(
    session_id: String,
    sink: StreamSink<SessionEvent>,
) -> Result<()> {
    session_service::subscribe_session_events(session_id, sink).await
}

/// 订阅设备剪贴板事件流（仅 `ClipboardChanged` 文本）。
///
/// 说明：
/// - 与会话主事件流解耦，避免上层为处理剪贴板而阻塞状态链路；
/// - 调用方取消 Dart 订阅后，Rust 转发线程会自动退出。
pub async fn subscribe_clipboard_events(
    session_id: String,
    sink: StreamSink<String>,
) -> Result<()> {
    session_service::subscribe_clipboard_events(session_id, sink).await
}

/// 订阅 Rust 日志流（FRB）。
///
/// 说明：
/// - 全局日志总线，不绑定单会话；
/// - 调用方取消 Dart 订阅后，Rust 转发线程会自动退出。
pub async fn subscribe_logs(sink: StreamSink<LogEvent>) -> Result<()> {
    session_service::subscribe_logs(sink).await
}

/// 初始化 YOLO 推理配置（仅硬件后端）。
///
/// 参数：
/// - `config`：初始推理配置（模型路径、输入尺寸、阈值、后端）。
pub async fn init_yolo(config: YoloConfig) -> Result<()> {
    crate::yolo::service::yolo_service::init_yolo(config).await
}

/// 运行中更新 YOLO 推理配置（实时生效）。
///
/// 参数：
/// - `config`：新的推理配置（模型路径、输入尺寸、阈值、后端）。
pub async fn update_yolo_config(config: YoloConfig) -> Result<()> {
    crate::yolo::service::yolo_service::update_yolo_config(config).await
}

/// 设置会话级 YOLO 开关。
///
/// 参数：
/// - `session_id`：目标会话 ID；
/// - `enabled`：`true` 启用，`false` 关闭。
pub async fn set_yolo_enabled(session_id: String, enabled: bool) -> Result<()> {
    crate::yolo::service::yolo_service::set_yolo_enabled(session_id, enabled).await
}

/// 订阅会话级 YOLO 结果流。
///
/// 参数：
/// - `session_id`：目标会话 ID；
/// - `sink`：FRB 结果流下沉通道。
pub async fn subscribe_yolo_results(
    session_id: String,
    sink: StreamSink<YoloFrameResult>,
) -> Result<()> {
    crate::yolo::service::yolo_service::subscribe_yolo_results(session_id, sink).await
}
