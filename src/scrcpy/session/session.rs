use std::collections::VecDeque;

use crate::gh_common::{Result, ScrcpyError};
use crate::gh_common::model::{ErrorCode, SessionEvent};
use crate::scrcpy::client::scrcpy_conn::ScrcpyConnect;
use crate::scrcpy::client::scrcpy_control::{
    ControlChannel, KeyEvent, ScrollEvent, TouchEvent,
};
use crate::scrcpy::client::scrcpy_video_stream::FramedVideoStreamReader;
use tracing::{debug, info, warn};

/// 会话本地事件队列上限。
///
/// 设计说明：
/// - 仅保存最近事件，避免异常风暴导致内存持续增长；
/// - 项目级事件持久化由 runtime 层负责。
const SESSION_EVENT_QUEUE_LIMIT: usize = 256;

/// 单条 scrcpy 会话。
///
/// 职责说明：
/// - 只承载“已连接成功”的会话资源；
/// - 持有视频读取器、控制通道和底层连接对象；
/// - 对外提供控制命令与资源销毁能力；
/// - 在会话级别产出 `SessionEvent`（不负责全局分发）。
pub struct Session {
    /// 底层连接对象，用于会话销毁时停止 server 与清理端口。
    conn: ScrcpyConnect,
    /// 控制通道。
    control: Option<ControlChannel>,
    /// 视频分帧读取器。
    video_stream: Option<FramedVideoStreamReader>,
    /// 会话本地事件队列。
    events: VecDeque<SessionEvent>,
    /// 是否已经执行销毁。
    disposed: bool,
}

impl Session {
    /// 仅供 crate 内部构建“已连接成功”的会话对象。
    ///
    /// 注意：
    /// - 调用方必须先完成 deploy/start/connect/read_header；
    /// - 外部模块不能直接构建未连接会话。
    pub(crate) fn from_connected(
        conn: ScrcpyConnect,
        control: ControlChannel,
        video_stream: FramedVideoStreamReader,
    ) -> Self {
        info!("[会话] 创建已连接会话对象");
        Self {
            conn,
            control: Some(control),
            video_stream: Some(video_stream),
            events: VecDeque::with_capacity(SESSION_EVENT_QUEUE_LIMIT),
            disposed: false,
        }
    }

    /// 向会话本地队列写入一条事件。
    fn push_event(&mut self, event: SessionEvent) {
        self.events.push_back(event);
        while self.events.len() > SESSION_EVENT_QUEUE_LIMIT {
            let _ = self.events.pop_front();
        }
    }

    /// 拉取并清空会话本地事件。
    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        let mut out = Vec::with_capacity(self.events.len());
        while let Some(event) = self.events.pop_front() {
            out.push(event);
        }
        out
    }

    /// 销毁会话并释放底层资源。
    ///
    /// 行为说明：
    /// - 清空会话内存资源引用；
    /// - 调用底层连接对象停止 server；
    /// - 重复调用安全。
    pub async fn dispose(&mut self) -> Result<()> {
        if self.disposed {
            warn!("[会话] 重复调用 dispose，已忽略");
            return Ok(());
        }

        info!("[会话] 开始销毁会话资源");
        self.video_stream = None;
        self.control = None;
        self.conn.stop().await?;
        self.disposed = true;
        self.push_event(SessionEvent::Stopped);
        info!("[会话] 销毁完成");
        Ok(())
    }

    /// 返回会话是否已销毁。
    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    /// 获取视频读取器的可变引用。
    pub fn video_stream_mut(&mut self) -> Result<&mut FramedVideoStreamReader> {
        if self.disposed {
            warn!("[会话] 获取视频流读取器失败：会话已销毁");
            return Err(ScrcpyError::Other("session has been disposed".to_string()));
        }

        self.video_stream
            .as_mut()
            .ok_or_else(|| ScrcpyError::Other("session video stream is not ready".to_string()))
    }

    /// 获取控制通道的可变引用。
    pub fn control_channel_mut(&mut self) -> Result<&mut ControlChannel> {
        if self.disposed {
            warn!("[会话] 获取控制通道失败：会话已销毁");
            return Err(ScrcpyError::Other("session has been disposed".to_string()));
        }

        self.control
            .as_mut()
            .ok_or_else(|| ScrcpyError::Other("session control channel is not ready".to_string()))
    }

    /// 发送触摸事件。
    pub async fn send_touch(&mut self, event: &TouchEvent) -> Result<()> {
        debug!("[会话] 发送触摸事件");
        let ret = self.control_channel_mut()?.send_touch_event(event).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("send_touch failed: {}", e),
            });
            warn!("[会话] 触摸事件发送失败: {}", e);
        }
        ret
    }

    /// 发送按键事件。
    pub async fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        debug!("[会话] 发送按键事件");
        let ret = self.control_channel_mut()?.send_key_event(event).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("send_key failed: {}", e),
            });
            warn!("[会话] 按键事件发送失败: {}", e);
        }
        ret
    }

    /// 发送滚轮事件。
    pub async fn send_scroll(&mut self, event: &ScrollEvent) -> Result<()> {
        debug!("[会话] 发送滚轮事件");
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
            warn!("[会话] 滚轮事件发送失败: {}", e);
        }
        ret
    }

    /// 发送文本输入。
    pub async fn send_text(&mut self, text: &str) -> Result<()> {
        debug!("[会话] 发送文本输入，长度={}", text.len());
        let ret = self.control_channel_mut()?.send_text(text).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("send_text failed: {}", e),
            });
            warn!("[会话] 文本输入发送失败: {}", e);
        }
        ret
    }

    /// 设置设备剪贴板内容。
    pub async fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        debug!("[会话] 设置剪贴板，长度={}, paste={}", text.len(), paste);
        let ret = self.control_channel_mut()?.set_clipboard(text, paste).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("set_clipboard failed: {}", e),
            });
            warn!("[会话] 设置剪贴板失败: {}", e);
        }
        ret
    }

    /// 请求设备熄灭或点亮物理屏幕。
    pub async fn set_display_power(&mut self, on: bool) -> Result<()> {
        info!("[会话] 设置显示电源状态: on={}", on);
        let ret = self.control_channel_mut()?.set_display_power(on).await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("set_display_power failed: {}", e),
            });
            warn!("[会话] 设置显示电源失败: {}", e);
        }
        ret
    }

    /// 请求服务端尽快输出新的关键帧。
    pub async fn request_idr(&mut self) -> Result<()> {
        info!("[会话] 请求关键帧（IDR）");
        let ret = self.control_channel_mut()?.send_reset_video().await;
        if let Err(e) = &ret {
            self.push_event(SessionEvent::Error {
                code: ErrorCode::ControlFailed,
                message: format!("request_idr failed: {}", e),
            });
            warn!("[会话] 请求关键帧失败: {}", e);
        }
        ret
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        debug!("[会话] 触发 drop，释放本地资源引用");
        self.video_stream = None;
        self.control = None;
    }
}


