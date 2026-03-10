//! scrcpy 控制通道实现。
//!
//! 作用：
//! - 把上层触控/键盘/滚轮/文本输入转换成 scrcpy 控制协议二进制消息；
//! - 通过 TCP 控制通道发送到设备端 server；
//! - 对发送失败统一返回 `ScrcpyError::Network`，由上层决定重试或重连。
use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use crate::gh_common::{Result, ScrcpyError};
use tracing::{debug, error, info, warn};
use serde::{Deserialize, Serialize};

// scrcpy控制消息类型（基于scrcpy 3.x协议）
// 参考：https://github.com/Genymobile/scrcpy/blob/master/app/src/control_msg.h
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum ControlMessageType {
    InjectKeycode = 0,
    InjectText = 1,
    InjectTouch = 2,
    InjectScroll = 3,
    // SetScreenPowerMode = 4,
    // ExpandNotificationPanel = 5,
    // CollapseNotificationPanel = 6,
    // GetClipboard = 7,
    // 注意：scrcpy server 协议中 SetClipboard 是 9，不是 8。
    SetClipboard = 9,
    SetDisplayPower = 10,
    /// 切换设备方向（仅切换，不是“绝对设为横/竖屏”）。
    RotateDevice = 11,
    /// 请求重置视频流（促发新的关键帧/参数集）。
    ResetVideo = 17,
    // SetScreenPowerModeExpanded = 9,
    UhidCreate = 12,
    UhidInput = 13,
    // OpenHardKeyboardSettings = 14,
    UhidDestroy = 14,
    // StartApp = 16,
}

// Android触摸事件动作
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum AndroidMotionEventAction {
    Down = 0,        // ACTION_DOWN
    Up = 1,          // ACTION_UP
    Move = 2,        // ACTION_MOVE
    Cancel = 3,      // ACTION_CANCEL
    PointerDown = 5, // ACTION_POINTER_DOWN
    PointerUp = 6,   // ACTION_POINTER_UP
    HoverMove = 7,   // ACTION_HOVER_MOVE (官方scrcpy用于鼠标移动)
    HoverEnter = 9,  // ACTION_HOVER_ENTER
    HoverExit = 10,  // ACTION_HOVER_EXIT
}

// 手动实现 Serialize 和 Deserialize，支持数字形式
impl serde::Serialize for AndroidMotionEventAction {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> serde::Deserialize<'de> for AndroidMotionEventAction {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        match value {
            0 => Ok(AndroidMotionEventAction::Down),
            1 => Ok(AndroidMotionEventAction::Up),
            2 => Ok(AndroidMotionEventAction::Move),
            3 => Ok(AndroidMotionEventAction::Cancel),
            5 => Ok(AndroidMotionEventAction::PointerDown),
            6 => Ok(AndroidMotionEventAction::PointerUp),
            7 => Ok(AndroidMotionEventAction::HoverMove),
            9 => Ok(AndroidMotionEventAction::HoverEnter),
            10 => Ok(AndroidMotionEventAction::HoverExit),
            _ => Err(serde::de::Error::custom(format!("Invalid action value: {}", value))),
        }
    }
}

// Android键盘事件动作
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum AndroidKeyEventAction {
    Down = 0,  // ACTION_DOWN
    Up = 1,    // ACTION_UP
}

// 手动实现 Serialize 和 Deserialize，支持数字形式
impl serde::Serialize for AndroidKeyEventAction {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> serde::Deserialize<'de> for AndroidKeyEventAction {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        match value {
            0 => Ok(AndroidKeyEventAction::Down),
            1 => Ok(AndroidKeyEventAction::Up),
            _ => Err(serde::de::Error::custom(format!("Invalid key action value: {}", value))),
        }
    }
}

// 触摸事件消息（从WebSocket接收）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchEvent {
    pub action: AndroidMotionEventAction,
    pub pointer_id: i64,  // 官方使用int64_t，支持POINTER_ID_MOUSE=-1, POINTER_ID_GENERIC_FINGER=-2
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub width: u32,
    pub height: u32,
    pub buttons: u32,
}

// 键盘事件消息（从WebSocket接收）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEvent {
    pub action: AndroidKeyEventAction,
    pub keycode: u32,
    pub repeat: u32,
    pub metastate: u32,
}

// 滚动事件（从WebSocket接收）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollEvent {
    pub x: f32,           // 归一化坐标 [0, 1]
    pub y: f32,           // 归一化坐标 [0, 1]
    pub width: u32,       // 视频宽度
    pub height: u32,      // 视频高度
    pub hscroll: i32,     // 水平滚动量
    pub vscroll: i32,     // 垂直滚动量
}

pub struct ControlChannel {
    /// 与 scrcpy server 的控制写通道。
    ///
    /// 说明：
    /// - 该连接与视频流连接分离；
    /// - 读通道由后台 reader task 独立处理。
    writer: OwnedWriteHalf,
    /// 设备消息接收通道（reader task -> ControlChannel）。
    device_msg_rx: UnboundedReceiver<DeviceMessage>,
    /// 控制通道 reader task 句柄（会话销毁时 abort）。
    reader_task: JoinHandle<()>,
    /// 已收到 ACK 但尚未被 wait 消费的序号集合。
    acked_sequences: HashSet<u64>,
    /// 从设备收到但尚未被 Session 消费的剪贴板更新。
    pending_clipboards: VecDeque<String>,
    /// 本地递增序号，用于 clipboard ACK 同步。
    next_sequence: u64,
}

/// 控制通道设备侧消息。
#[derive(Debug, Clone)]
enum DeviceMessage {
    Clipboard(String),
    AckClipboard(u64),
}

impl ControlChannel {
    /// 创建控制通道。
    ///
    /// 参数 `stream` 必须是已经和 scrcpy server 建立成功的控制连接。
    pub fn new(stream: TcpStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel::<DeviceMessage>();
        let reader_task = tokio::spawn(async move {
            Self::run_device_message_reader(read_half, tx).await;
        });

        Self {
            writer: write_half,
            device_msg_rx: rx,
            reader_task,
            acked_sequences: HashSet::new(),
            pending_clipboards: VecDeque::new(),
            next_sequence: 1,
        }
    }

    /// 后台读取设备消息（剪贴板与 ACK）。
    async fn run_device_message_reader(
        mut reader: OwnedReadHalf,
        tx: UnboundedSender<DeviceMessage>,
    ) {
        loop {
            let mut ty = [0u8; 1];
            if let Err(e) = reader.read_exact(&mut ty).await {
                debug!("[控制通道] 设备消息读取结束: {}", e);
                break;
            }

            match ty[0] {
                // DEVICE_MSG_TYPE_CLIPBOARD
                0 => {
                    let mut len_buf = [0u8; 4];
                    if let Err(e) = reader.read_exact(&mut len_buf).await {
                        warn!("[控制通道] 读取剪贴板长度失败: {}", e);
                        break;
                    }
                    let text_len = u32::from_be_bytes(len_buf) as usize;
                    let mut text_buf = vec![0u8; text_len];
                    if let Err(e) = reader.read_exact(&mut text_buf).await {
                        warn!("[控制通道] 读取剪贴板内容失败: {}", e);
                        break;
                    }
                    let text = String::from_utf8_lossy(&text_buf).to_string();
                    let _ = tx.send(DeviceMessage::Clipboard(text));
                }
                // DEVICE_MSG_TYPE_ACK_CLIPBOARD
                1 => {
                    let mut seq_buf = [0u8; 8];
                    if let Err(e) = reader.read_exact(&mut seq_buf).await {
                        warn!("[控制通道] 读取剪贴板 ACK 失败: {}", e);
                        break;
                    }
                    let sequence = u64::from_be_bytes(seq_buf);
                    let _ = tx.send(DeviceMessage::AckClipboard(sequence));
                }
                // DEVICE_MSG_TYPE_UHID_OUTPUT（当前仅跳过，避免协议流错位）
                2 => {
                    let mut header = [0u8; 4];
                    if let Err(e) = reader.read_exact(&mut header).await {
                        warn!("[控制通道] 读取 UHID 输出头失败: {}", e);
                        break;
                    }
                    let size = u16::from_be_bytes([header[2], header[3]]) as usize;
                    let mut payload = vec![0u8; size];
                    if let Err(e) = reader.read_exact(&mut payload).await {
                        warn!("[控制通道] 读取 UHID 输出数据失败: {}", e);
                        break;
                    }
                }
                other => {
                    warn!("[控制通道] 收到未知设备消息类型: {}", other);
                    break;
                }
            }
        }
    }

    /// 拉取控制通道中的设备消息并更新本地缓存。
    pub fn poll_device_messages(&mut self) {
        while let Ok(msg) = self.device_msg_rx.try_recv() {
            match msg {
                DeviceMessage::Clipboard(text) => {
                    self.pending_clipboards.push_back(text);
                }
                DeviceMessage::AckClipboard(seq) => {
                    self.acked_sequences.insert(seq);
                }
            }
        }
    }

    /// 取出设备侧剪贴板更新（由 Session 转发到上层）。
    pub fn take_clipboard_updates(&mut self) -> Vec<String> {
        self.poll_device_messages();
        let mut out = Vec::with_capacity(self.pending_clipboards.len());
        while let Some(text) = self.pending_clipboards.pop_front() {
            out.push(text);
        }
        out
    }

    fn alloc_sequence(&mut self) -> u64 {
        let seq = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        seq
    }

    async fn wait_clipboard_ack(&mut self, sequence: u64, timeout: Duration) -> Result<()> {
        self.poll_device_messages();
        if self.acked_sequences.remove(&sequence) {
            return Ok(());
        }

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ScrcpyError::Network(format!(
                    "等待剪贴板 ACK 超时: sequence={}",
                    sequence
                )));
            }
            let remain = deadline - now;
            match tokio::time::timeout(remain, self.device_msg_rx.recv()).await {
                Ok(Some(DeviceMessage::AckClipboard(seq))) => {
                    if seq == sequence {
                        return Ok(());
                    }
                    self.acked_sequences.insert(seq);
                }
                Ok(Some(DeviceMessage::Clipboard(text))) => {
                    self.pending_clipboards.push_back(text);
                }
                Ok(None) => {
                    return Err(ScrcpyError::Network(
                        "控制通道设备消息接收器已关闭".to_string(),
                    ));
                }
                Err(_) => {
                    return Err(ScrcpyError::Network(format!(
                        "等待剪贴板 ACK 超时: sequence={}",
                        sequence
                    )));
                }
            }
        }
    }

    /// 发送触摸事件到设备
    /// scrcpy 3.x 触摸消息格式（32字节）：
    /// [type:1][action:1][pointer_id:8][x:4][y:4][width:2][height:2][pressure:2][action_button:4][buttons:4]
    /// 所有多字节字段都是大端序(Big Endian)
    /// pressure使用16位定点数(u16fp): float * 0xFFFF
    /// 官方源码确认：return 32 (不是33或36)
    pub async fn send_touch_event(&mut self, event: &TouchEvent) -> Result<()> {
        debug!("🖐️  Sending touch event: {:?}", event);

        let mut msg = Vec::with_capacity(32);  // 官方确认：32字节

        // 1. 消息类型 (1 byte) = InjectTouch (2)
        msg.push(ControlMessageType::InjectTouch as u8);

        // 2. 动作 (1 byte)
        msg.push(event.action as u8);

        // 3. pointer_id (8 bytes, Big Endian, signed int64)
        msg.extend_from_slice(&event.pointer_id.to_be_bytes());

        // 4. x坐标 (4 bytes, Big Endian, 像素坐标)
        // 上游已传入设备像素坐标，这里只做边界裁剪，避免重复缩放。
        let x_fixed = event
            .x
            .clamp(0.0, (event.width.saturating_sub(1)) as f32)
            .round() as u32;
        msg.extend_from_slice(&x_fixed.to_be_bytes());

        // 5. y坐标 (4 bytes, Big Endian, 像素坐标)
        // 上游已传入设备像素坐标，这里只做边界裁剪，避免重复缩放。
        let y_fixed = event
            .y
            .clamp(0.0, (event.height.saturating_sub(1)) as f32)
            .round() as u32;
        msg.extend_from_slice(&y_fixed.to_be_bytes());

        // 6. 屏幕宽度 (2 bytes, Big Endian)
        msg.extend_from_slice(&(event.width as u16).to_be_bytes());

        // 7. 屏幕高度 (2 bytes, Big Endian)
        msg.extend_from_slice(&(event.height as u16).to_be_bytes());

        // 8. 压力 (2 bytes, Big Endian, 16位定点数)
        // 官方scrcpy使用0xffff表示1.0，0x0000表示0.0
        let pressure_u16 = (event.pressure.clamp(0.0, 1.0) * 0xFFFF as f32) as u16;
        msg.extend_from_slice(&pressure_u16.to_be_bytes());

        // 9. action_button (4 bytes, Big Endian)
        // 根据官方scrcpy抓包分析：
        // - 鼠标模式（pointer_id=-1）：action_button 始终为 1（LEFT_BUTTON）
        // - 触摸模式（pointer_id>=0）：action_button 为 0
        let action_button = if event.pointer_id == -1 {
            1u32  // 鼠标模式：始终为 1
        } else {
            0u32  // 触摸模式
        };
        msg.extend_from_slice(&action_button.to_be_bytes());

        // 10. 按钮状态 (4 bytes, Big Endian)
        // 根据官方scrcpy抓包：
        // - 鼠标模式（pointer_id=-1）：
        //   DOWN/MOVE: buttons=1
        //   UP: buttons=0
        // - 触摸模式（pointer_id>=0）：buttons=0
        let buttons = if event.pointer_id == -1 {
            // 鼠标模式：UP事件必须为0，其他事件使用前端传来的值
            match event.action {
                AndroidMotionEventAction::Up | AndroidMotionEventAction::PointerUp => 0u32,
                _ => event.buttons,
            }
        } else {
            // 触摸模式：buttons 始终为 0
            0u32
        };
        msg.extend_from_slice(&buttons.to_be_bytes());

        debug!("📤 Touch message ({} bytes): action={:?}, x={}/{}, y={}/{}, pressure={} (u16=0x{:04x}), action_button={}, buttons={}",
            msg.len(), event.action, x_fixed, event.width, y_fixed, event.height, event.pressure, pressure_u16, action_button, buttons);
        debug!("   Complete message bytes: {:02x?}", msg);

        match self.writer.write_all(&msg).await {
            Ok(_) => {
                debug!("✅ TCP write successful");
            }
            Err(e) => {
                error!("❌ TCP write failed: {}", e);
                return Err(ScrcpyError::Network(format!("Failed to send touch event: {}", e)));
            }
        }

        match self.writer.flush().await {
            Ok(_) => {
                debug!("✅ TCP flush successful");
            }
            Err(e) => {
                error!("❌ TCP flush failed: {}", e);
                return Err(ScrcpyError::Network(format!("刷新控制通道失败: {}", e)));
            }
        }

        Ok(())
    }

    /// 发送按键事件到设备
    /// scrcpy 3.x 按键消息格式：
    /// [type=0][action][keycode][repeat][metastate]
    pub async fn send_key_event(&mut self, event: &KeyEvent) -> Result<()> {
        debug!("⌨️  Sending key event: {:?}", event);

        let mut msg = Vec::with_capacity(14);

        // 1. 消息类型 (1 byte) = InjectKeycode (0)
        msg.push(ControlMessageType::InjectKeycode as u8);

        // 2. 动作 (1 byte)
        msg.push(event.action as u8);

        // 3. keycode (4 bytes, Big Endian)
        msg.extend_from_slice(&event.keycode.to_be_bytes());

        // 4. repeat (4 bytes, Big Endian)
        msg.extend_from_slice(&event.repeat.to_be_bytes());

        // 5. metastate (4 bytes, Big Endian)
        msg.extend_from_slice(&event.metastate.to_be_bytes());

        debug!("📤 Key message ({} bytes): {:02x?}", msg.len(), msg);

        self.writer.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to send key event: {}", e)))?;

        self.writer.flush().await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        Ok(())
    }

    /// 发送滚动事件到设备
    /// scrcpy 3.x 滚动消息格式 (21 bytes)：
    /// [type=3][x:4][y:4][width:2][height:2][hscroll:2][vscroll:2][buttons:4]
    ///
    /// 根据官方 scrcpy 抓包分析：
    /// - 滚动值使用 i16 定点数格式
    /// - 向下滚动: vscroll = 0xf800 (-2048)
    /// - 向上滚动: vscroll = 0x0800 (2048)
    /// - 前端传入 -1/0/1，需要乘以 2048 转换
    pub async fn send_scroll_event(
        &mut self,
        x: f32,
        y: f32,
        width: u32,
        height: u32,
        hscroll: i32,
        vscroll: i32,
    ) -> Result<()> {
        debug!("📜 Sending scroll event: x={}, y={}, h={}, v={}", x, y, hscroll, vscroll);

        let mut msg = Vec::with_capacity(21);

        // 1. 消息类型 (1 byte) = InjectScroll (3)
        msg.push(ControlMessageType::InjectScroll as u8);

        // 2. x坐标 (4 bytes, Big Endian, i32)
        let x_fixed = (x * width as f32) as i32;
        msg.extend_from_slice(&x_fixed.to_be_bytes());

        // 3. y坐标 (4 bytes, Big Endian, i32)
        let y_fixed = (y * height as f32) as i32;
        msg.extend_from_slice(&y_fixed.to_be_bytes());

        // 4. 屏幕宽度 (2 bytes, Big Endian)
        msg.extend_from_slice(&(width as u16).to_be_bytes());

        // 5. 屏幕高度 (2 bytes, Big Endian)
        msg.extend_from_slice(&(height as u16).to_be_bytes());

        // 6. 水平滚动 (2 bytes, Big Endian, i16)
        // 官方 scrcpy 使用 0x0800 (2048) 作为滚动单位
        // 前端传入 -1, 0, 1，需要乘以 2048
        let hscroll_i16 = (hscroll * 2048).clamp(-32768, 32767) as i16;
        msg.extend_from_slice(&hscroll_i16.to_be_bytes());

        // 7. 垂直滚动 (2 bytes, Big Endian, i16)
        let vscroll_i16 = (vscroll * 2048).clamp(-32768, 32767) as i16;
        msg.extend_from_slice(&vscroll_i16.to_be_bytes());

        // 8. 按钮状态 (4 bytes, Big Endian)
        msg.extend_from_slice(&0u32.to_be_bytes());

        debug!("📤 Scroll message ({} bytes): hscroll_i16={}, vscroll_i16={}, hex={:02x?}",
            msg.len(), hscroll_i16, vscroll_i16, msg);

        self.writer.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to send scroll event: {}", e)))?;

        self.writer.flush().await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        Ok(())
    }

    /// 发送返回键
    pub async fn send_back_key(&mut self) -> Result<()> {
        info!("◀️  Sending BACK key");

        // Android KEYCODE_BACK = 4
        self.send_key_event(&KeyEvent {
            action: AndroidKeyEventAction::Down,
            keycode: 4,
            repeat: 0,
            metastate: 0,
        }).await?;

        self.send_key_event(&KeyEvent {
            action: AndroidKeyEventAction::Up,
            keycode: 4,
            repeat: 0,
            metastate: 0,
        }).await?;

        Ok(())
    }

    /// 发送Home键
    pub async fn send_home_key(&mut self) -> Result<()> {
        info!("🏠 Sending HOME key");

        // Android KEYCODE_HOME = 3
        self.send_key_event(&KeyEvent {
            action: AndroidKeyEventAction::Down,
            keycode: 3,
            repeat: 0,
            metastate: 0,
        }).await?;

        self.send_key_event(&KeyEvent {
            action: AndroidKeyEventAction::Up,
            keycode: 3,
            repeat: 0,
            metastate: 0,
        }).await?;

        Ok(())
    }

    /// 发送文本注入事件（直接输入文字）
    /// scrcpy 3.x 文本消息格式：
    /// [type=1][length:4][text:variable]
    pub async fn send_text(&mut self, text: &str) -> Result<()> {
        // info!("📝 Sending text: {} chars", text.len());

        let text_bytes = text.as_bytes();
        let mut msg = Vec::with_capacity(5 + text_bytes.len());

        // 1. 消息类型 (1 byte) = InjectText (1)
        msg.push(ControlMessageType::InjectText as u8);

        // 2. 文本长度 (4 bytes, Big Endian)
        msg.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());

        // 3. 文本内容 (variable)
        msg.extend_from_slice(text_bytes);

        debug!("📤 Text message ({} bytes)", msg.len());

        self.writer.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to send text: {}", e)))?;

        self.writer.flush().await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        Ok(())
    }

    /// 设置设备剪贴板内容
    /// scrcpy 3.x 剪贴板消息格式：
    /// [type=9][sequence:8][paste:1][length:4][text:variable]
    pub async fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        info!("📋 Setting clipboard: {} chars, paste={}", text.len(), paste);

        let text_bytes = text.as_bytes();
        let mut msg = Vec::with_capacity(14 + text_bytes.len());

        // 1. 消息类型 (1 byte) = SetClipboard (9)
        msg.push(ControlMessageType::SetClipboard as u8);

        // 2. sequence (8 bytes, Big Endian)
        // - paste=false：走 ACK 同步，确保后续 Ctrl+V 不会粘贴旧内容；
        // - paste=true：不依赖 ACK，保持即时触发语义。
        let sequence = if paste { 0 } else { self.alloc_sequence() };
        msg.extend_from_slice(&sequence.to_be_bytes());

        // 3. paste标志 (1 byte) - 是否模拟粘贴操作
        msg.push(if paste { 1 } else { 0 });

        // 4. 文本长度 (4 bytes, Big Endian)
        msg.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());

        // 5. 文本内容 (variable)
        msg.extend_from_slice(text_bytes);

        debug!("📤 Clipboard message ({} bytes)", msg.len());

        self.writer.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to set clipboard: {}", e)))?;

        self.writer.flush().await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        if !paste {
            self.wait_clipboard_ack(sequence, Duration::from_millis(500))
                .await?;
            debug!("[控制通道] 已收到剪贴板 ACK: sequence={}", sequence);
        }

        Ok(())
    }

    /// 设置设备显示电源状态（scrcpy 控制协议 TYPE_SET_DISPLAY_POWER=10）。
    ///
    /// 关键语义：
    /// - `on = false`：请求“仅关闭设备物理屏幕显示”，不停止 scrcpy 视频采集；
    /// - `on = true`：请求打开设备物理屏幕显示。
    ///
    /// 与直接发送 `keyevent 223` 的区别：
    /// - keyevent 会让设备进入休眠流程，部分机型会中断视频链路；
    /// - set_display_power 是 scrcpy 官方协议，目标是“投屏继续、手机屏幕熄灭”。
    pub async fn set_display_power(&mut self, on: bool) -> Result<()> {
        let msg = [ControlMessageType::SetDisplayPower as u8, if on { 1 } else { 0 }];

        self.writer
            .write_all(&msg)
            .await
            .map_err(|e| ScrcpyError::Network(format!("Failed to set display power: {}", e)))?;

        self.writer
            .flush()
            .await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        Ok(())
    }

    /// 通过 scrcpy 协议发送“切换设备方向”请求。
    ///
    /// 协议格式：
    /// - 仅 1 字节类型值 `TYPE_ROTATE_DEVICE(11)`，无额外负载。
    ///
    /// 注意：
    /// - 该能力是“切换(toggle)”语义，不保证一次调用后必然达到指定方向；
    /// - 若需要“绝对方向”，应由上层结合当前分辨率/旋转状态决定是否发送。
    pub async fn send_rotate_device(&mut self) -> Result<()> {
        let msg = [ControlMessageType::RotateDevice as u8];
        self.writer
            .write_all(&msg)
            .await
            .map_err(|e| ScrcpyError::Network(format!("Failed to rotate device: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;
        Ok(())
    }

    /// 请求服务端重置视频流（提示尽快输出新的关键帧）。
    ///
    /// 协议格式：
    /// - 仅 1 字节类型值 `TYPE_RESET_VIDEO(17)`，无额外负载。
    pub async fn send_reset_video(&mut self) -> Result<()> {
        let msg = [ControlMessageType::ResetVideo as u8];
        self.writer
            .write_all(&msg)
            .await
            .map_err(|e| ScrcpyError::Network(format!("Failed to request reset video: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;
        Ok(())
    }

    /// 创建 UHID 键盘设备。
    ///
    /// 协议格式：
    /// [type=12][id:2][vendor_id:2][product_id:2][name_len:1][name][report_desc_len:2][report_desc]
    pub async fn send_uhid_create_keyboard(
        &mut self,
        id: u16,
        vendor_id: u16,
        product_id: u16,
        name: &str,
        report_desc: &[u8],
    ) -> Result<()> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u8::MAX as usize {
            return Err(ScrcpyError::Other(format!("UHID 键盘名称过长: {}", name_bytes.len())));
        }
        if report_desc.len() > u16::MAX as usize {
            return Err(ScrcpyError::Other(format!("UHID 报告描述符过长: {}", report_desc.len())));
        }

        let mut msg = Vec::with_capacity(10 + name_bytes.len() + report_desc.len());
        msg.push(ControlMessageType::UhidCreate as u8);
        msg.extend_from_slice(&id.to_be_bytes());
        msg.extend_from_slice(&vendor_id.to_be_bytes());
        msg.extend_from_slice(&product_id.to_be_bytes());
        msg.push(name_bytes.len() as u8);
        msg.extend_from_slice(name_bytes);
        msg.extend_from_slice(&(report_desc.len() as u16).to_be_bytes());
        msg.extend_from_slice(report_desc);

        info!("[UHID] 创建键盘设备: id={}, vendor_id={}, product_id={}, name_len={}, report_desc_len={}", id, vendor_id, product_id, name_bytes.len(), report_desc.len());

        self.writer
            .write_all(&msg)
            .await
            .map_err(|e| ScrcpyError::Network(format!("创建 UHID 键盘失败: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        Ok(())
    }

    /// 发送 UHID 输入报告。
    ///
    /// 协议格式：
    /// [type=13][id:2][data_len:2][data]
    pub async fn send_uhid_input(&mut self, id: u16, data: &[u8]) -> Result<()> {
        if data.len() > u16::MAX as usize {
            return Err(ScrcpyError::Other(format!("UHID 输入报告过长: {}", data.len())));
        }

        let mut msg = Vec::with_capacity(5 + data.len());
        msg.push(ControlMessageType::UhidInput as u8);
        msg.extend_from_slice(&id.to_be_bytes());
        msg.extend_from_slice(&(data.len() as u16).to_be_bytes());
        msg.extend_from_slice(data);

        debug!("[UHID] 发送输入报告: id={}, data_len={}", id, data.len());

        self.writer
            .write_all(&msg)
            .await
            .map_err(|e| ScrcpyError::Network(format!("发送 UHID 输入报告失败: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        Ok(())
    }

    /// 销毁 UHID 设备。
    ///
    /// 协议格式：
    /// [type=14][id:2]
    pub async fn send_uhid_destroy(&mut self, id: u16) -> Result<()> {
        let mut msg = Vec::with_capacity(3);
        msg.push(ControlMessageType::UhidDestroy as u8);
        msg.extend_from_slice(&id.to_be_bytes());

        info!("[UHID] 销毁设备: id={}", id);

        self.writer
            .write_all(&msg)
            .await
            .map_err(|e| ScrcpyError::Network(format!("销毁 UHID 设备失败: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| ScrcpyError::Network(format!("刷新控制通道失败: {}", e)))?;

        Ok(())
    }
}

impl Drop for ControlChannel {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}












