use async_trait::async_trait;
use tracing::{info, warn};

use crate::gh_common::{Result, ScrcpyError};
use crate::scrcpy::client::scrcpy_control::{
    ControlChannel, KeyEvent, ScrollEvent, TouchEvent,
};
use crate::scrcpy::input::input_client::ScrcpyInputClient;
use crate::scrcpy::input::uhid_keyboard_state::UhidKeyboardState;

/// 默认 UHID 设备 ID。
///
/// 说明：
/// - 同一控制通道内，create/input/destroy 必须使用同一个 id；
/// - 当前项目只创建一个虚拟键盘设备，固定使用 0x01。
const DEFAULT_UHID_DEVICE_ID: u16 = 0x01;

/// 默认厂商 ID（VID）。
///
/// 说明：
/// - 该值主要用于设备端标识，不影响键值映射行为；
/// - 这里沿用 Google 常见测试值，便于问题排查时识别来源。
const DEFAULT_UHID_VENDOR_ID: u16 = 0x18D1;

/// 默认产品 ID（PID）。
const DEFAULT_UHID_PRODUCT_ID: u16 = 0x4EE7;

/// 设备名称。
const DEFAULT_UHID_NAME: &str = "GameHelper UHID Keyboard";

/// 标准 Boot Keyboard 报告描述符（8字节输入报告）。
///
/// 关键点：
/// - 1字节修饰键位图（Ctrl/Shift/Alt/Meta）；
/// - 1字节保留位；
/// - 6字节普通按键槽位（最多6键同时按下）。
const KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01,
    0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
    0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

/// UHID 输入客户端。
///
/// 设计边界：
/// - 键盘事件：走 UHID 虚拟外设通道；
/// - 触摸/滚轮/文本/剪贴板/电源控制：仍走 scrcpy control channel；
/// - 不在本层处理“自动回退注入模式”，由更上层策略控制。
pub struct UhidInputClient {
    /// scrcpy 控制通道。
    control: ControlChannel,
    /// 本地键盘状态机（修饰键 + 6键槽位）。
    keyboard_state: UhidKeyboardState,
    /// UHID 设备标识。
    device_id: u16,
    /// 是否已执行 start/create。
    started: bool,
}

impl UhidInputClient {
    /// 创建 UHID 输入客户端。
    pub fn new(control: ControlChannel) -> Self {
        Self {
            control,
            keyboard_state: UhidKeyboardState::new(),
            device_id: DEFAULT_UHID_DEVICE_ID,
            started: false,
        }
    }

    /// 确保客户端已启动。
    ///
    /// 目的：
    /// - 防止在未 create 设备时发送 input 报告；
    /// - 把调用时序错误尽早暴露给上层。
    fn ensure_started(&self) -> Result<()> {
        if self.started {
            Ok(())
        } else {
            Err(ScrcpyError::Other("UHID input client not started".to_string()))
        }
    }

    /// 将当前键盘状态编码为报告并发送。
    async fn send_current_report(&mut self) -> Result<()> {
        let report = self.keyboard_state.to_report();
        self.control.send_uhid_input(self.device_id, &report).await
    }
}

#[async_trait]
impl ScrcpyInputClient for UhidInputClient {
    /// 启动 UHID 键盘。
    ///
    /// 流程：
    /// 1. 发送 create_keyboard；
    /// 2. 发送一次“全释放”报告，避免历史脏状态；
    /// 3. 标记 started=true。
    async fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }

        self.control
            .send_uhid_create_keyboard(
                self.device_id,
                DEFAULT_UHID_VENDOR_ID,
                DEFAULT_UHID_PRODUCT_ID,
                DEFAULT_UHID_NAME,
                KEYBOARD_REPORT_DESCRIPTOR,
            )
            .await?;

        self.keyboard_state.release_all();
        self.send_current_report().await?;
        self.started = true;
        info!("[UHID] 已创建虚拟键盘设备");
        Ok(())
    }

    /// 停止 UHID 键盘。
    ///
    /// 流程：
    /// 1. 尝试发送全释放报告，尽量避免设备端残留按键；
    /// 2. 发送 destroy；
    /// 3. 本地置 started=false。
    async fn stop(&mut self) -> Result<()> {
        if !self.started {
            return Ok(());
        }

        self.keyboard_state.release_all();
        if let Err(e) = self.send_current_report().await {
            warn!("[UHID] 停止时发送释放报告失败: {}", e);
        }

        if let Err(e) = self.control.send_uhid_destroy(self.device_id).await {
            warn!("[UHID] 销毁虚拟设备失败: {}", e);
        }

        self.started = false;
        Ok(())
    }

    /// 触摸事件沿用 control channel。
    async fn send_touch(&mut self, event: &TouchEvent) -> Result<()> {
        self.control.send_touch_event(event).await
    }

    /// 键盘事件通过 UHID 报告发送。
    async fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        self.ensure_started()?;
        self.keyboard_state.update_key(event.action, event.keycode);
        self.send_current_report().await
    }

    /// 滚轮事件沿用 control channel。
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

    /// 文本注入沿用 control channel。
    async fn send_text(&mut self, text: &str) -> Result<()> {
        self.control.send_text(text).await
    }

    /// 剪贴板沿用 control channel。
    async fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        self.control.set_clipboard(text, paste).await
    }

    /// 屏幕电源控制沿用 control channel。
    async fn set_display_power(&mut self, on: bool) -> Result<()> {
        self.control.set_display_power(on).await
    }

    /// 旋转请求沿用 control channel。
    async fn rotate_device(&mut self) -> Result<()> {
        self.control.send_rotate_device().await
    }

    /// 请求关键帧沿用 control channel。
    async fn request_idr(&mut self) -> Result<()> {
        self.control.send_reset_video().await
    }
}
