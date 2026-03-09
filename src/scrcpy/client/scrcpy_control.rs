//! scrcpy 控制通道实现。
//!
//! 作用：
//! - 把上层触控/键盘/滚轮/文本输入转换成 scrcpy 控制协议二进制消息；
//! - 通过 TCP 控制通道发送到设备端 server；
//! - 对发送失败统一返回 `ScrcpyError::Network`，由上层决定重试或重连。
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use crate::gh_common::{Result, ScrcpyError};
use tracing::{info, debug, error};
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
    SetClipboard = 8,
    SetDisplayPower = 10,
    // SetScreenPowerModeExpanded = 9,
    // RotateDevice = 10,
    // UhidCreate = 11,
    // UhidInput = 12,
    // OpenHardKeyboardSettings = 13,
    // UhidDestroy = 14,
    // StartApp = 15,
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
    /// 与 scrcpy server 的控制 socket。
    ///
    /// 说明：
    /// - 该连接与视频流连接分离；
    /// - 会话销毁时由 Session 统一释放。
    stream: TcpStream,
}

impl ControlChannel {
    /// 创建控制通道。
    ///
    /// 参数 `stream` 必须是已经和 scrcpy server 建立成功的控制连接。
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
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

        match self.stream.write_all(&msg).await {
            Ok(_) => {
                debug!("✅ TCP write successful");
            }
            Err(e) => {
                error!("❌ TCP write failed: {}", e);
                return Err(ScrcpyError::Network(format!("Failed to send touch event: {}", e)));
            }
        }

        match self.stream.flush().await {
            Ok(_) => {
                debug!("✅ TCP flush successful");
            }
            Err(e) => {
                error!("❌ TCP flush failed: {}", e);
                return Err(ScrcpyError::Network(format!("Failed to flush control stream: {}", e)));
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

        self.stream.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to send key event: {}", e)))?;

        self.stream.flush().await
            .map_err(|e| ScrcpyError::Network(format!("Failed to flush control stream: {}", e)))?;

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

        self.stream.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to send scroll event: {}", e)))?;

        self.stream.flush().await
            .map_err(|e| ScrcpyError::Network(format!("Failed to flush control stream: {}", e)))?;

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
        info!("📝 Sending text: {} chars", text.len());

        let text_bytes = text.as_bytes();
        let mut msg = Vec::with_capacity(5 + text_bytes.len());

        // 1. 消息类型 (1 byte) = InjectText (1)
        msg.push(ControlMessageType::InjectText as u8);

        // 2. 文本长度 (4 bytes, Big Endian)
        msg.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());

        // 3. 文本内容 (variable)
        msg.extend_from_slice(text_bytes);

        debug!("📤 Text message ({} bytes)", msg.len());

        self.stream.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to send text: {}", e)))?;

        self.stream.flush().await
            .map_err(|e| ScrcpyError::Network(format!("Failed to flush control stream: {}", e)))?;

        Ok(())
    }

    /// 设置设备剪贴板内容
    /// scrcpy 3.x 剪贴板消息格式：
    /// [type=8][sequence:8][paste:1][length:4][text:variable]
    pub async fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        info!("📋 Setting clipboard: {} chars, paste={}", text.len(), paste);

        let text_bytes = text.as_bytes();
        let mut msg = Vec::with_capacity(14 + text_bytes.len());

        // 1. 消息类型 (1 byte) = SetClipboard (8)
        msg.push(ControlMessageType::SetClipboard as u8);

        // 2. sequence (8 bytes, Big Endian) - 用于同步，这里使用0
        msg.extend_from_slice(&0u64.to_be_bytes());

        // 3. paste标志 (1 byte) - 是否模拟粘贴操作
        msg.push(if paste { 1 } else { 0 });

        // 4. 文本长度 (4 bytes, Big Endian)
        msg.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());

        // 5. 文本内容 (variable)
        msg.extend_from_slice(text_bytes);

        debug!("📤 Clipboard message ({} bytes)", msg.len());

        self.stream.write_all(&msg).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to set clipboard: {}", e)))?;

        self.stream.flush().await
            .map_err(|e| ScrcpyError::Network(format!("Failed to flush control stream: {}", e)))?;

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

        self.stream
            .write_all(&msg)
            .await
            .map_err(|e| ScrcpyError::Network(format!("Failed to set display power: {}", e)))?;

        self.stream
            .flush()
            .await
            .map_err(|e| ScrcpyError::Network(format!("Failed to flush control stream: {}", e)))?;

        Ok(())
    }

    /// 请求服务端重置视频流（提示尽快输出新的关键帧）。
    ///
    /// 兼容性说明：
    /// - 部分 scrcpy 版本未公开该控制消息类型；
    /// - 为避免影响连接稳定性，这里采用“安全 no-op”策略返回成功；
    /// - 后续若统一了 server 侧协议，可在此替换为真实消息发送。
    pub async fn send_reset_video(&mut self) -> Result<()> {
        Ok(())
    }
}






