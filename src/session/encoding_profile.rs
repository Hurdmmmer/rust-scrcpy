use serde::{Deserialize, Serialize};

/// 视频编码参数配置（会话域模型）。
///
/// 说明：
/// - 该结构体由 `SessionManager` 在启动 scrcpy server 时消费；
/// - 字段基本一一映射到 scrcpy server 参数；
/// - 统一放入 `session` 目录，避免“策略目录”与会话实现分离导致理解成本上升。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodingProfile {
    /// 视频长边最大尺寸，0 表示不限制。
    pub max_size: u32,
    /// 视频码率（bit/s）。
    pub bit_rate: u32,
    /// 最大帧率，0 表示由设备/编码器自行决定。
    pub max_fps: u32,
    /// 关键帧间隔（秒），用于旋转/场景突变后的快速恢复。
    pub intra_refresh_period: u32,
    /// 指定编码器名称，None 表示自动选择。
    pub video_encoder: Option<String>,
    /// 建链后是否关闭手机物理屏幕（仅保留投屏流）。
    pub turn_screen_off: bool,
    /// 建链期间是否保持唤醒。
    pub stay_awake: bool,
    /// 启动时是否强制横屏。
    pub force_landscape: bool,
    /// scrcpy server 日志级别。
    pub scrcpy_log_level: String,
}

impl Default for EncodingProfile {
    fn default() -> Self {
        Self {
            max_size: 0,
            bit_rate: 8_000_000,
            max_fps: 60,
            intra_refresh_period: 1,
            video_encoder: None,
            turn_screen_off: false,
            stay_awake: false,
            force_landscape: false,
            scrcpy_log_level: "info".to_string(),
        }
    }
}
