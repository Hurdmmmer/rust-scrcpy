use async_trait::async_trait;

use crate::gh_common::Result;
use crate::scrcpy::client::scrcpy_control::{KeyEvent, ScrollEvent, TouchEvent};

/// 会话输入客户端统一抽象。
///
/// 说明：
/// - start/stop 对应输入子系统生命周期；
/// - send_* 对应会话期输入事件下发；
/// - 所有实现都应保证失败返回明确错误，不允许静默吞错。
#[async_trait]
pub trait ScrcpyInputClient: Send {
    /// 输入子系统启动。
    async fn start(&mut self) -> Result<()>;

    /// 输入子系统停止与清理。
    async fn stop(&mut self) -> Result<()>;

    /// 发送触摸事件。
    async fn send_touch(&mut self, event: &TouchEvent) -> Result<()>;

    /// 发送按键事件。
    async fn send_key(&mut self, event: &KeyEvent) -> Result<()>;

    /// 发送滚轮事件。
    async fn send_scroll(&mut self, event: &ScrollEvent) -> Result<()>;

    /// 发送文本输入。
    async fn send_text(&mut self, text: &str) -> Result<()>;

    /// 设置剪贴板内容。
    async fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()>;

    /// 设置显示电源状态。
    async fn set_display_power(&mut self, on: bool) -> Result<()>;

    /// 切换设备方向（协议 rotate）。
    async fn rotate_device(&mut self) -> Result<()>;

    /// 请求关键帧（IDR）。
    async fn request_idr(&mut self) -> Result<()>;
}
