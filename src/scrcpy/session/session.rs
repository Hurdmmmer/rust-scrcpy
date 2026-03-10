use std::collections::VecDeque;

use tracing::{debug, info, warn};

use crate::gh_common::model::{ErrorCode, SessionEvent};
use crate::gh_common::{Result, ScrcpyError};
use crate::scrcpy::client::scrcpy_conn::ScrcpyConnect;
use crate::scrcpy::client::scrcpy_control::{ControlChannel, KeyEvent, ScrollEvent, TouchEvent};
use crate::scrcpy::client::scrcpy_video_stream::FramedVideoStreamReader;
use crate::scrcpy::input::{ScrcpyInputMode, UhidKeyboardState};

const SESSION_EVENT_QUEUE_LIMIT: usize = 256;
const DEFAULT_UHID_DEVICE_ID: u16 = 0x01;
const DEFAULT_UHID_VENDOR_ID: u16 = 0x18D1;
const DEFAULT_UHID_PRODUCT_ID: u16 = 0x4EE7;
const DEFAULT_UHID_NAME: &str = "GameHelper UHID Keyboard";

// 标准 Boot Keyboard 报告描述符。
const KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01,
    0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
    0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

/// 单条 scrcpy 会话。
///
/// 职责：
/// - 持有连接、控制通道、视频流；
/// - 维护会话事件队列；
/// - 承载键盘输入后端策略（Inject/UHID/Auto）。
pub struct Session {
    conn: ScrcpyConnect,
    control: Option<ControlChannel>,
    video_stream: Option<FramedVideoStreamReader>,
    events: VecDeque<SessionEvent>,
    disposed: bool,

    input_mode: ScrcpyInputMode,
    uhid_started: bool,
    uhid_keyboard_state: UhidKeyboardState,
    uhid_device_id: u16,
}

impl Session {
    /// 仅供内部构建“已连接成功”的会话对象。
    pub(crate) fn from_connected(
        conn: ScrcpyConnect,
        control: ControlChannel,
        video_stream: FramedVideoStreamReader,
        input_mode: ScrcpyInputMode,
    ) -> Self {
        info!("[会话] 创建已连接会话，input_mode={:?}", input_mode);
        Self {
            conn,
            control: Some(control),
            video_stream: Some(video_stream),
            events: VecDeque::with_capacity(SESSION_EVENT_QUEUE_LIMIT),
            disposed: false,
            input_mode,
            uhid_started: false,
            uhid_keyboard_state: UhidKeyboardState::new(),
            uhid_device_id: DEFAULT_UHID_DEVICE_ID,
        }
    }

    fn push_event(&mut self, event: SessionEvent) {
        self.events.push_back(event);
        while self.events.len() > SESSION_EVENT_QUEUE_LIMIT {
            let _ = self.events.pop_front();
        }
    }

    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        let mut out = Vec::with_capacity(self.events.len());
        while let Some(event) = self.events.pop_front() {
            out.push(event);
        }
        out
    }

    pub async fn dispose(&mut self) -> Result<()> {
        if self.disposed {
            warn!("[会话] 重复调用 dispose，已忽略");
            return Ok(());
        }

        info!("[会话] 开始销毁会话资源");

        if self.uhid_started {
            self.uhid_keyboard_state.release_all();
            let _ = self.send_uhid_report().await;
            let device_id = self.uhid_device_id;
            let _ = self
                .control_channel_mut()?
                .send_uhid_destroy(device_id)
                .await;
            self.uhid_started = false;
        }

        self.video_stream = None;
        self.control = None;
        self.conn.stop().await?;
        self.disposed = true;
        self.push_event(SessionEvent::Stopped);
        info!("[会话] 资源销毁完成");
        Ok(())
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    pub fn video_stream_mut(&mut self) -> Result<&mut FramedVideoStreamReader> {
        if self.disposed {
            return Err(ScrcpyError::Other("session has been disposed".to_string()));
        }

        self.video_stream
            .as_mut()
            .ok_or_else(|| ScrcpyError::Other("session video stream is not ready".to_string()))
    }

    pub fn control_channel_mut(&mut self) -> Result<&mut ControlChannel> {
        if self.disposed {
            return Err(ScrcpyError::Other("session has been disposed".to_string()));
        }

        self.control
            .as_mut()
            .ok_or_else(|| ScrcpyError::Other("session control channel is not ready".to_string()))
    }

    /// 会话建链完成后预热输入后端。
    ///
    /// 设计目标：
    /// - 强制 UHID 时在连接阶段完成设备创建，避免首键延迟；
    /// - Auto 模式若预热失败则回退 Inject，避免影响触摸与基础输入。
    pub async fn warmup_input_backend(&mut self) -> Result<()> {
        match self.input_mode {
            ScrcpyInputMode::Inject => Ok(()),
            ScrcpyInputMode::Uhid => self.ensure_uhid_ready().await,
            ScrcpyInputMode::Auto => {
                if let Err(err) = self.ensure_uhid_ready().await {
                    warn!("[会话] UHID 预热失败，回退 Inject: {}", err);
                    self.input_mode = ScrcpyInputMode::Inject;
                }
                Ok(())
            }
        }
    }

    async fn ensure_uhid_ready(&mut self) -> Result<()> {
        if self.uhid_started {
            return Ok(());
        }

        let device_id = self.uhid_device_id;
        self.control_channel_mut()?
            .send_uhid_create_keyboard(
                device_id,
                DEFAULT_UHID_VENDOR_ID,
                DEFAULT_UHID_PRODUCT_ID,
                DEFAULT_UHID_NAME,
                KEYBOARD_REPORT_DESCRIPTOR,
            )
            .await?;

        self.uhid_keyboard_state.release_all();
        self.send_uhid_report().await?;
        self.uhid_started = true;
        info!("[会话] UHID 键盘已创建");
        Ok(())
    }

    async fn send_uhid_report(&mut self) -> Result<()> {
        let report = self.uhid_keyboard_state.to_report();
        let device_id = self.uhid_device_id;
        self.control_channel_mut()?
            .send_uhid_input(device_id, &report)
            .await
    }

    pub async fn send_touch(&mut self, event: &TouchEvent) -> Result<()> {
        let ret = self.control_channel_mut()?.send_touch_event(event).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("send_touch failed: {}", e),
            });
        }
        ret
    }

    pub async fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        let ret = match self.input_mode {
            ScrcpyInputMode::Inject => self.control_channel_mut()?.send_key_event(event).await,
            ScrcpyInputMode::Uhid => {
                self.ensure_uhid_ready().await?;
                self.uhid_keyboard_state
                    .update_key(event.action, event.keycode);
                self.send_uhid_report().await
            }
            ScrcpyInputMode::Auto => {
                let uhid_try = async {
                    self.ensure_uhid_ready().await?;
                    self.uhid_keyboard_state
                        .update_key(event.action, event.keycode);
                    self.send_uhid_report().await
                }
                .await;

                match uhid_try {
                    Ok(_) => Ok(()),
                    Err(err) => {
                        warn!("[会话] UHID 键盘发送失败，回退 Inject: {}", err);
                        self.input_mode = ScrcpyInputMode::Inject;
                        self.control_channel_mut()?.send_key_event(event).await
                    }
                }
            }
        };

        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("send_key failed: {}", e),
            });
        }
        ret
    }

    pub async fn send_scroll(&mut self, event: &ScrollEvent) -> Result<()> {
        let ret = self
            .control_channel_mut()?
            .send_scroll_event(
                event.x,
                event.y,
                event.width,
                event.height,
                event.hscroll,
                event.vscroll,
            )
            .await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("send_scroll failed: {}", e),
            });
        }
        ret
    }

    pub async fn send_text(&mut self, text: &str) -> Result<()> {
        let ret = self.control_channel_mut()?.send_text(text).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("send_text failed: {}", e),
            });
        }
        ret
    }

    pub async fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        let ret = self.control_channel_mut()?.set_clipboard(text, paste).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("set_clipboard failed: {}", e),
            });
        }
        ret
    }

    pub async fn set_display_power(&mut self, on: bool) -> Result<()> {
        let ret = self.control_channel_mut()?.set_display_power(on).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("set_display_power failed: {}", e),
            });
        }
        ret
    }

    pub async fn rotate_device(&mut self) -> Result<()> {
        let ret = self.control_channel_mut()?.send_rotate_device().await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("rotate_device failed: {}", e),
            });
        }
        ret
    }

    pub async fn request_idr(&mut self) -> Result<()> {
        let ret = self.control_channel_mut()?.send_reset_video().await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("request_idr failed: {}", e),
            });
        }
        ret
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        debug!("[会话] 触发 drop，释放本地引用");
        self.video_stream = None;
        self.control = None;
    }
}


