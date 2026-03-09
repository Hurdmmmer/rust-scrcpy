use serde::{Deserialize, Serialize};

/// Scrcpy 客户端配置。
///
/// 设计目标：
/// 1. 统一承载“连接建立 + 编码参数 + 运行策略”所需配置；
/// 2. 作为 `ScrcpyClient::new()` 的唯一入参，避免散乱参数传递；
/// 3. 字段语义与现网参数保持一致，便于从旧实现平滑迁移。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrcpyClientConfig {
    /// adb 可执行文件路径。
    pub adb_path: String,
    /// scrcpy-server 本地路径。
    pub server_path: String,
    /// 目标设备 ID（adb serial）。
    pub device_id: String,
    /// 请求视频端口（实际使用端口可能自动后移）。
    pub video_port: u16,
    /// 请求控制端口（实际使用端口可能自动后移）。
    pub control_port: u16,
    /// 视频长边最大尺寸，0 表示不限制。
    pub max_size: u32,
    /// 视频码率（bit/s）。
    pub bit_rate: u32,
    /// 最大帧率，0 表示不限制。
    pub max_fps: u32,
    /// 强制关键帧间隔（秒），0 表示交由编码器策略控制。
    pub intra_refresh_period: u32,
    /// 指定编码器名称，None 表示自动选择。
    pub video_encoder: Option<String>,
    /// 建链后是否熄灭设备物理屏幕。
    pub turn_screen_off: bool,
    /// 建链后是否保持设备防休眠。
    pub stay_awake: bool,
    /// scrcpy server 日志级别（例如 info/debug）。
    pub scrcpy_log_level: String,
}

impl Default for ScrcpyClientConfig {
    /// 提供一组安全默认值，便于测试与最小启动。
    fn default() -> Self {
        Self {
            adb_path: String::new(),
            server_path: String::new(),
            device_id: String::new(),
            video_port: 27183,
            control_port: 27184,
            max_size: 0,
            bit_rate: 8_000_000,
            max_fps: 60,
            intra_refresh_period: 1,
            video_encoder: None,
            turn_screen_off: false,
            stay_awake: false,
            scrcpy_log_level: "info".to_string(),
        }
    }
}

