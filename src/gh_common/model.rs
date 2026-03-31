use flutter_rust_bridge::frb;
use serde::{Deserialize, Serialize};

/// 设备信息（面向 Flutter 的稳定返回模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// ADB 设备序列号。
    pub device_id: String,
    /// 设备型号，例如 `SM-G9810`。
    pub model: String,
    /// Android 版本，例如 `13`。
    pub android_version: String,
    /// 屏幕宽度（像素）。
    pub width: u32,
    /// 屏幕高度（像素）。
    pub height: u32,
    /// 可选 IP（Wi-Fi 设备时可填）。
    pub ip: Option<String>,
}

/// 会话事件/调用错误码（供上层统一处理）。
#[frb(unignore)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ErrorCode {
    InvalidSession,
    AlreadyRunning,
    NotRunning,
    DeviceDisconnected,
    DecodeFailed,
    TextureFailed,
    ControlFailed,
    Internal,
}

/// DLL 侧日志级别。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Rust -> Dart 日志事件模型（FRB 流传输）。
#[frb(unignore)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    /// 日志级别文本（trace/debug/info/warn/error）。
    pub level: String,
    /// 日志来源 target。
    pub target: String,
    /// 日志正文。
    pub message: String,
    /// UTC 毫秒时间戳。
    pub ts_millis: i64,
}

/// 创建会话所需配置（与 Flutter 桥接模型对齐）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// adb 可执行文件路径。
    pub adb_path: String,
    /// scrcpy-server 路径。
    pub server_path: String,
    /// 目标设备 ID。
    pub device_id: String,
    /// 最大分辨率（长边），0 表示不限制。
    pub max_size: u32,
    /// 视频码率（bps）。
    pub bit_rate: u32,
    /// 最大帧率，0 表示不限制。
    pub max_fps: u32,
    /// 视频端口。
    pub video_port: u16,
    /// 控制端口。
    pub control_port: u16,
    /// 指定编码器名称（可选）。
    pub video_encoder: Option<String>,
    /// 是否在会话启动后关闭设备屏幕。
    pub turn_screen_off: bool,
    /// 是否保持设备防休眠。
    pub stay_awake: bool,
    /// scrcpy 日志级别字符串（例如 `info`）。
    pub scrcpy_verbosity: String,
    /// 强制生成 IDR 关键帧的周期（秒），0 表示由编码器自己决定。
    pub intra_refresh_period: u32,
}

/// 渲染链路模式（新增配置，仅用于 V2 API）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RenderPipelineMode {
    /// 维持现有共享句柄链路。
    Original,
    /// V2：纯 CPU BGRA + PixelBuffer 链路（不使用共享纹理渲染）。
    CpuPixelBufferV2,
}

/// 解码器选择模式（新增配置，仅用于 V2 API）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DecoderMode {
    PreferHardware,
    ForceHardware,
    ForceSoftware,
}

/// 创建会话所需扩展配置（V2）。
///
/// 说明：
/// - `base` 与旧版 `SessionConfig` 完全一致；
/// - `render_pipeline_mode`/`decoder_mode` 仅影响新 API；
/// - 旧 API 不读取这些字段，保证兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfigV2 {
    pub base: SessionConfig,
    pub render_pipeline_mode: RenderPipelineMode,
    pub decoder_mode: DecoderMode,
}

/// 会话方向模式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrientationMode {
    Auto,
    Portrait,
    Landscape,
}

/// 方向变更来源。
#[frb(unignore)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrientationChangeSource {
    /// 上层 API 主动发起。
    ManualApi,
    /// 系统自动旋转（传感器/系统策略）。
    AutoSensor,
}

/// 会话状态事件流。
#[frb(unignore)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    Starting,
    Running,
    Reconnecting,
    Stopped,
    Error {
        code: ErrorCode,
        message: String,
    },
    OrientationChanged {
        mode: OrientationMode,
        source: OrientationChangeSource,
    },
    ResolutionChanged {
        width: u32,
        height: u32,
        new_handle: i64,
        generation: u64,
    },
}

/// 会话统计信息（供上层监控与调试）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub fps: f64,
    pub decode_latency_ms: u32,
    pub upload_latency_ms: u32,
    pub total_frames: u64,
    pub dropped_frames: u64,
}

/// 系统按键类型（语义键）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SystemKey {
    Home,
    Back,
    Recent,
    PowerMenu,
    VolumeUp,
    VolumeDown,
    RotateScreen,
}

/// 纹理帧元信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextureFrame {
    pub handle: i64,
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub pts: i64,
}

/// Android 触摸事件动作（Flutter API 输入模型）。
#[repr(u8)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AndroidMotionEventAction {
    Down = 0,
    Up = 1,
    Move = 2,
    Cancel = 3,
    PointerDown = 5,
    PointerUp = 6,
    HoverMove = 7,
    HoverEnter = 9,
    HoverExit = 10,
}

/// Android 按键事件动作（Flutter API 输入模型）。
#[repr(u8)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AndroidKeyEventAction {
    Down = 0,
    Up = 1,
}

/// 触摸事件（Flutter API 输入模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchEvent {
    pub action: AndroidMotionEventAction,
    pub pointer_id: i64,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub width: u32,
    pub height: u32,
    pub buttons: u32,
}

/// 按键事件（Flutter API 输入模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEvent {
    pub action: AndroidKeyEventAction,
    pub keycode: u32,
    pub repeat: u32,
    pub metastate: u32,
}

/// 滚轮事件（Flutter API 输入模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollEvent {
    pub x: f32,
    pub y: f32,
    pub width: u32,
    pub height: u32,
    pub hscroll: i32,
    pub vscroll: i32,
}

/// YOLO 推理执行后端（仅硬件后端，不允许 CPU 推理）。
#[frb(unignore)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum YoloExecutionProvider {
    /// Windows DirectML（兼容 NVIDIA/AMD/Intel）。
    DirectMl,
    /// NVIDIA CUDA（依赖 CUDA 环境）。
    Cuda,
    /// NVIDIA TensorRT（高性能，部署要求更高）。
    TensorRt,
}

/// YOLO 推理配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoloConfig {
    /// ONNX 模型绝对路径。
    pub model_path: String,
    /// 网络输入宽度（例如 640）。
    pub input_width: u32,
    /// 网络输入高度（例如 640）。
    pub input_height: u32,
    /// 置信度阈值。
    pub confidence_threshold: f32,
    /// NMS IoU 阈值。
    pub iou_threshold: f32,
    /// 每帧最大检测框数量。
    pub max_detections: u32,
    /// 推理后端（仅硬件后端）。
    pub provider: YoloExecutionProvider,
    /// 可选设备索引（CUDA/TensorRT 场景可用）。
    pub device_index: Option<u32>,
    /// 推理限频（每秒最多处理多少帧）。
    pub max_infer_fps: u32,
}

/// 单个检测框结果。
#[frb(unignore)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoloDetection {
    /// 类别 ID。
    pub class_id: u32,
    /// 类别名称（可选）。
    pub label: Option<String>,
    /// 置信度分数。
    pub score: f32,
    /// 左上角 x（像素坐标）。
    pub x: f32,
    /// 左上角 y（像素坐标）。
    pub y: f32,
    /// 宽度（像素）。
    pub width: f32,
    /// 高度（像素）。
    pub height: f32,
}

/// 单帧 YOLO 推理结果事件。
#[frb(unignore)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoloFrameResult {
    /// 关联会话 ID。
    pub session_id: String,
    /// 视频帧 ID。
    pub frame_id: u64,
    /// 原始帧宽度。
    pub frame_width: u32,
    /// 原始帧高度。
    pub frame_height: u32,
    /// 推理耗时（毫秒）。
    pub infer_latency_ms: u32,
    /// 检测框列表。
    pub detections: Vec<YoloDetection>,
}
