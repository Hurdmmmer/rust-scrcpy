/// 会话默认使用 scrcpy framed 协议。
pub const DEFAULT_USE_FRAMED_STREAM: bool = true;

/// 指定硬解名称（`None` 表示自动探测）。
pub const REQUESTED_HW_DECODER: Option<&str> = None;

/// 是否禁用 `h264_cuvid`（默认不禁用）。
pub const DISABLE_CUVID: bool = false;
