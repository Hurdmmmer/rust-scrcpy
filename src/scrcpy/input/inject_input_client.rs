use async_trait::async_trait;

use crate::gh_common::Result;
use crate::scrcpy::client::scrcpy_control::{ControlChannel, KeyEvent, ScrollEvent, TouchEvent};
use crate::scrcpy::input::input_client::ScrcpyInputClient;

/// Inject 输入客户端（现网稳定实现）。
///
/// 说明：
/// - 直接复用现有 ControlChannel 注入协议；
/// - 该实现用于当前生产默认链路；
/// - 后续 UHID 失败回退也复用该实现。
pub struct InjectInputClient {
    /// scrcpy 控制通道。
    control: ControlChannel,
}

impl InjectInputClient {
    /// 基于现有控制通道构建 Inject 输入客户端。
    pub fn new(control: ControlChannel) -> Self {
        Self { control }
    }
}

#[async_trait]
impl ScrcpyInputClient for InjectInputClient {
    async fn start(&mut self) -> Result<()> {
        // Inject 模式无需额外握手，直接可用。
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        // 由会话统一销毁连接，这里不做额外动作。
        Ok(())
    }

    async fn send_touch(&mut self, event: &TouchEvent) -> Result<()> {
        self.control.send_touch_event(event).await
    }

    async fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        self.control.send_key_event(event).await
    }

    async fn send_scroll(&mut self, event: &ScrollEvent) -> Result<()> {
        self.control
            .send_scroll_event(
                event.x,
                event.y,
                event.width,
                event.height,
                event.hscroll,
                event.vscroll,
            )
            .await
    }

    async fn send_text(&mut self, text: &str) -> Result<()> {
        self.control.send_text(text).await
    }

    async fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        self.control.set_clipboard(text, paste).await
    }

    async fn set_display_power(&mut self, on: bool) -> Result<()> {
        self.control.set_display_power(on).await
    }

    async fn rotate_device(&mut self) -> Result<()> {
        self.control.send_rotate_device().await
    }

    async fn request_idr(&mut self) -> Result<()> {
        self.control.send_reset_video().await
    }
}
