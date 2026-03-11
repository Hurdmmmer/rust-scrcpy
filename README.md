其他依赖：**adb**（任意版本）、**scrcpy-server**（v3.3.4，只能这个版本，其他版本协议不对）、**FFmpeg 运行时 DLL**（Windows 运行时必需）。*adb 和 scrcpy-server 注意路径，--help 中会有提示*

<p align="center">
  <img width="220"
       src="https://github.com/user-attachments/assets/4f6ab07c-c84e-43bc-a889-0d07eb22db18" />
</p>
<p align="center">
  <sub>🦀 && 🤖</sub><br/>
</p>



## 当前运行模式（重要）

当前仓库代码主链路为 **Rust 动态库（cdylib）+ Flutter FFI API**：

- 对外 API：`src/gh_api/flutter_api.rs`
- 服务层：`src/scrcpy/scrcpy_service.rs`
- 核心运行时：`src/scrcpy/runtime/scrcpy_core_runtime.rs`
- 会话层：`src/scrcpy/session/session.rs`、`src/scrcpy/session/session_manager.rs`
- 连接与控制：`src/scrcpy/client/scrcpy_client.rs`、`src/scrcpy/client/scrcpy_conn.rs`、`src/scrcpy/client/scrcpy_control.rs`
- 解码链路：`src/scrcpy/runtime/scrcpy_decode_pipeline.rs`、`src/scrcpy/decode_core/*`

> 下文原有 scrcpy 协议参数、架构、连接与调优章节继续完整保留；仅对与当前代码不一致的字段做修正。

## 安装与运行（Rust 侧）

### 1) 必需文件

- `adb` 可执行文件
- `scrcpy-server-v3.3.4`
- Windows 下 FFmpeg DLL：
  - `avcodec-62.dll`
  - `avformat-62.dll`
  - `avutil-60.dll`
  - `swresample-6.dll`
  - `swscale-9.dll`

DLL 放置位置（任选其一）：

1. 与最终可执行文件同目录
2. 与 `rust_scrcpy.dll` 同目录
3. 系统 `PATH` 目录

### 2) 构建命令（Rust）

```bash
cargo build --release --lib --features frb
```

### 3) 典型调用执行流程（当前代码）

1. `setup_logger(max_level)`
2. `list_devices(adb_path)`
3. `create_session_v2(config)`（或 `create_session`）
4. `start_session(session_id)`
5. 运行期控制：`send_touch/send_key/send_scroll/send_text/send_system_key/set_clipboard/set_orientation_mode/request_idr`
6. `get_session_stats(session_id)`
7. `stop_session(session_id)`
8. `dispose_session(session_id)`

## 目录

1. [系统概述](#1-系统概述)
2. [整体架构](#2-整体架构)
3. [启动流程](#3-启动流程)
4. [ADB通信层](#4-adb通信层)
5. [Scrcpy Server协议](#5-scrcpy-server协议)
6. [视频流处理](#6-视频流处理)
7. [控制流处理](#7-控制流处理)
   - [7.7 文本注入协议](#77-文本注入协议)
   - [7.8 剪贴板设置协议](#78-剪贴板设置协议)
   - [7.9 统一控制事件类型](#79-统一控制事件类型)
   - [7.10 键盘输入支持](#710-键盘输入支持)
   - [7.11 剪贴板粘贴功能](#711-剪贴板粘贴功能)
   - [7.12 鼠标滚轮支持](#712-鼠标滚轮支持)
   - [7.5 屏幕旋转自动适配](#75-屏幕旋转自动适配)
8. [解码与渲染链路](#9-解码与渲染链路当前实现)
9. [数据流转图](#10-数据流转图)
10. [关键技术细节](#11-关键技术细节)
11. [配置参数说明](#12-配置参数说明)
12. [错误处理](#13-错误处理)
13. [端口自动寻找机制](#14-端口自动寻找机制)
---

## 1. 系统概述

Rust-Scrcpy 是一个用 Rust 实现的 Android 屏幕镜像系统，通过 ADB 与设备通信，使用 scrcpy-server 捕获屏幕，并通过 ADB 与设备通信，拉起 scrcpy-server 传输视频与控制流，并通过 FFI 向上层应用暴露会话与控制能力。

### 核心特性

- **实时屏幕镜像**: 低延迟 H.264 视频流传输
- **解码策略**: FFmpeg 硬解优先，可按策略强制硬解/软解
- **双向控制**: 支持触摸、鼠标、按键事件（仅单点控制）
- **键盘输入**: 支持字母、数字、功能键、方向键等
- **剪贴板粘贴**: 支持 Ctrl+V 快速粘贴文本到手机
- **鼠标滚轮**: 支持滚轮滚动，方便浏览网页和列表
- **屏幕旋转适配**: 自动检测横竖屏切换并调整显示
- **自动 IDR 帧请求**: 新客户端连接时自动获取关键帧，提高画面响应速度
- **自动端口**：自动跳过占用的端口，使用未被占用的端口

### 技术栈

| 组件           | 技术                                                  |
| -------------- | -----------------------------------------------------|
| 后端运行时     | Tokio (异步)                                           |
| API 边界       | FFI (`gh_api/flutter_api`)                           |
| 视频编码       | H.264 (Android MediaCodec)                            |
| 解码实现       | ffmpeg-next（硬解优先，失败回退软解）                     |
| 进程通信       | ADB forward + TCP                                     |

---

## 2. 整体架构

```
┌─────────────────────────────────────────────────────────────────────────┐
│                       rust-ws-scrcpy 当前架构（库模式）                    │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ┌──────────────────────────────┐                                       │
│  │   Upper App (FFI Caller)     │                                       │
│  │  - setup_logger              │                                       │
│  │  - create/start/stop session │                                       │
│  │  - send_* controls           │                                       │
│  └──────────────┬───────────────┘                                       │
│                 │                                                       │
│                 ▼                                                       │
│  ┌──────────────────────────────┐                                       │
│  │ gh_api::flutter_api (Facade) │                                       │
│  │  src/gh_api/flutter_api.rs   │                                       │
│  └──────────────┬───────────────┘                                       │
│                 │                                                       │
│                 ▼                                                       │
│  ┌──────────────────────────────┐                                       │
│  │ service + runtime            │                                       │
│  │  service.rs / runtime.rs     │                                       │
│  │  RealSessionRuntime::start() │                                       │
│  └──────────────┬───────────────┘                                       │
│                 │                                                       │
│                 ▼                                                       │
│  ┌──────────────────────────────┐                                       │
│  │ SessionManager::connect_v2   │                                       │
│  │  - ScrcpyServer.deploy/start │                                       │
│  │  - connect video/control     │                                       │
│  └──────────────┬───────────────┘                                       │
│                 │ adb push / adb forward / adb shell                    │
│                 ▼                                                       │
│  ┌──────────────────────────────┐                                       │
│  │ Android Device               │                                       │
│  │  scrcpy-server v3.3.4        │                                       │
│  │  video socket + control      │                                       │
│  └──────────────┬───────────────┘                                       │
│                 │                                                       │
│                 ▼                                                       │
│  ┌──────────────────────────────┐                                       │
│  │ FramedVideoStreamReader      │                                       │
│  │ DecoderPipeline (FFmpeg)     │                                       │
│  │ ControlChannel               │                                       │
│  └──────────────────────────────┘                                       │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---
## 3. 启动流程

### 3.1 完整启动时序图（当前实现）

```
┌────────────┐  ┌───────────────────────┐  ┌────────────────┐  ┌──────────────┐  ┌──────────────┐
│ Upper App  │  │ gh_api/flutter_api    │  │ CoreRuntime    │  │ ScrcpyServer │  │ AndroidDevice│
└─────┬──────┘  └──────────┬────────────┘  └───────┬────────┘  └──────┬───────┘  └──────┬───────┘
      │ setup_logger()               │                     │                 │                 │
      │──────────────────────────────>│                     │                 │                 │
      │ list_devices(adb_path)        │                     │                 │                 │
      │──────────────────────────────>│                     │                 │                 │
      │<──────────────────────────────│                     │                 │                 │
      │ create_session_v2(config)     │                     │                 │                 │
      │──────────────────────────────>│                     │                 │                 │
      │ start_session(session_id)      │                    │                 │                 │
      │──────────────────────────────>│  start()            │                 │                 │
      │                               │────────────────────>│ connect_v2()    │                 │
      │                               │                     │────────────────>│ deploy/start     │
      │                               │                     │                 │────────────────>│
      │                               │                     │                 │ adb push/forward │
      │                               │                     │                 │ adb shell server │
      │                               │                     │ connect video/control             │
      │                               │                     │────────────────>│                 │
      │                               │                     │ read_video_header(dummy/meta)    │
      │                               │                     │────────────────>│                 │
      │                               │                     │ start decoder/control loop        │
      │ send_touch/send_key/...       │                     │                 │                 │
      │──────────────────────────────>│────────────────────>│────────────────>│────────────────>│
      │ request_idr()                 │────────────────────>│────────────────>│────────────────>│
      │ get_session_stats()           │────────────────────>│                 │                 │
      │<──────────────────────────────│                     │                 │                 │
      │ stop_session()/dispose_session│                     │                 │                 │
      │──────────────────────────────>│────────────────────>│ stop()          │ stop + cleanup  │
```
### 3.2 启动代码流程（当前 API 路径）

```rust
// 伪代码：src/gh_api/flutter_api.rs 对外调用顺序
async fn start_flow(adb_path: String, config: SessionConfigV2) -> Result<()> {
    setup_logger(LogLevel::Info).await?;

    let devices = list_devices(adb_path).await?;
    if devices.is_empty() {
        return Err(ScrcpyError::DeviceNotFound);
    }

    let session_id = create_session_v2(config).await?;
    start_session(session_id.clone()).await?;

    // 运行期控制
    // send_touch(...).await?;
    // send_key(...).await?;
    // request_idr(session_id.clone()).await?;

    let _stats = get_session_stats(session_id.clone()).await?;

    stop_session(session_id.clone()).await?;
    dispose_session(session_id).await?;
    Ok(())
}
```

---
### 3.3 ADB WiFi 连接（通用）

步骤：

1. **首次设置（需要 USB）**

```bash
adb tcpip 5555
```

2. 查询手机 IP：

```bash
adb shell ip route | findstr wlan
```

3. WiFi 连接：

```bash
adb connect 192.168.1.xxx:5555
adb devices
```

**注意事项：**

- 手机和电脑需要在同一局域网。
- WiFi 连接延迟通常高于 USB。
- 某些手机重启后需要重新执行 `adb tcpip 5555`。

## 4. ADB通信层

### 4.1 AdbClient 实现

```rust
// src/adb/client.rs
pub struct AdbClient {
    pub adb_path: PathBuf,  // ADB 可执行文件路径
}

impl AdbClient {
    /// 执行 ADB 命令并返回输出
    pub async fn execute(&self, args: &[&str]) -> Result<String>;

    /// 获取已连接的设备列表
    pub async fn list_devices(&self) -> Result<Vec<String>>;

    /// 推送文件到设备
    pub async fn push(&self, device_id: &str, local: &str, remote: &str) -> Result<()>;

    /// 执行 shell 命令
    pub async fn shell(&self, device_id: &str, command: &str) -> Result<String>;

    /// 设置端口转发
    pub async fn forward(&self, device_id: &str, local_port: u16, remote: &str) -> Result<()>;

    /// 移除端口转发
    pub async fn forward_remove(&self, device_id: &str, local_port: u16) -> Result<()>;
}
```

### 4.2 关键 ADB 命令

| 命令          | 用途            | 示例                                                |
| ------------- | --------------- | --------------------------------------------------- |
| `adb devices` | 列出已连接设备  | `adb devices`                                       |
| `adb push`    | 推送文件到设备  | `adb -s xxx push server.jar /data/local/tmp/`       |
| `adb shell`   | 执行 shell 命令 | `adb -s xxx shell wm size`                          |
| `adb forward` | 端口转发        | `adb -s xxx forward tcp:27183 localabstract:scrcpy` |

### 4.3 端口转发机制

```
PC 端                                    Android 设备端
┌─────────────────┐                     ┌─────────────────────────────┐
│                 │                     │                             │
│  127.0.0.1:27183├────── USB/WiFi ────►│ localabstract:scrcpy        │
│  (视频流)        │      ADB Forward    │ (Unix Abstract Socket)       │
│                 │                     │                             │
│  127.0.0.1:27184├────── USB/WiFi ────►│ localabstract:scrcpy        │
│  (控制流)        │      ADB Forward    │ (同一个Socket,不同连接)       │
│                 │                     │                             │
└─────────────────┘                     └─────────────────────────────┘
```

**重要说明**: 视频流和控制流使用同一个 `localabstract:scrcpy` socket，但是是两个独立的 TCP 连接。scrcpy-server 会根据连接顺序区分：第一个连接是视频流，第二个连接是控制流。

---

## 5. Scrcpy Server协议

### 5.1 Server 启动参数

```bash
CLASSPATH=/data/local/tmp/scrcpy-server.jar \
app_process / com.genymobile.scrcpy.Server 3.3.4 \
    log_level=info \
    max_size=1920 \           # 最大分辨率
    video_bit_rate=4000000 \  # 视频码率 (默认：4Mbps)
    max_fps=60 \              # 最大帧率
    video_codec_options=i-frame-interval=1 \  # IDR帧间隔(默认：1秒)
    tunnel_forward=true \     # 使用端口转发模式
    send_device_meta=false \  # 不发送设备元数据
    send_frame_meta=false \   # 不发送帧元数据
    send_dummy_byte=true \    # 发送 dummy byte
    send_codec_meta=false \   # 不发送编解码器元数据
    raw_stream=true \         # 原始 NAL 流模式
    audio=false \             # 禁用音频
    control=true \            # 启用控制
    cleanup=true              # 退出时清理
```

### 5.2 raw_stream 模式协议

当 `raw_stream=true` 时，视频流格式非常简单：

```
┌─────────────────────────────────────────────────────────────┐
│                    Video Stream Format                      │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌──────────┐                                               │
│  │ Dummy    │  1 byte (0x00)                                │
│  │ Byte     │  表示连接建立成功                              │
│  └──────────┘                                               │
│       │                                                     │
│       ▼                                                     │
│  ┌──────────────────────────────────────────────────────┐   │
│  │              Annex-B H.264 NAL Stream                │   │
│  │                                                      │   │
│  │  ┌────────────┬─────────────────────────────────┐    │   │
│  │  │ Start Code │         NAL Unit Data           │    │   │
│  │  │ 00 00 01   │  [NAL Header][RBSP Payload]     │    │   │
│  │  └────────────┴─────────────────────────────────┘    │   │
│  │                        │                             │   │
│  │  ┌────────────┬────────▼────────────────────────┐    │   │
│  │  │ Start Code │         NAL Unit Data           │    │   │
│  │  │ 00 00 00 01│  [NAL Header][RBSP Payload]     │    │   │
│  │  └────────────┴─────────────────────────────────┘    │   │
│  │                        │                             │   │
│  │                       ...                            │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### 5.3 双连接模式

scrcpy 3.x 在 `control=true` 模式下需要两个连接：

```
连接顺序:
1. 第一个 TCP 连接 → 视频流 (Video Socket)
2. 第二个 TCP 连接 → 控制流 (Control Socket)

重要: Server 会等待两个连接都建立后才开始发送数据！
```

---

## 6. 视频流处理

### 6.1 H.264 NAL 单元类型

| NAL Type | 名称          | 说明                |
| -------- | ------------- | ------------------- |
| 1        | Non-IDR Slice | P/B 帧 (需要参考帧) |
| 5        | IDR Slice     | 关键帧 (独立解码)   |
| 6        | SEI           | 补充增强信息        |
| 7        | SPS           | 序列参数集          |
| 8        | PPS           | 图像参数集          |

### 6.2 VideoStreamReader 实现

```rust
// src/scrcpy/video.rs
pub struct VideoStreamReader {
    stream: TcpStream,
    buffer: BytesMut,          // 1MB 读取缓冲区
    frame_count: u64,
    first_start_code_pos: Option<usize>,  // 第一个起始码位置
}

impl VideoStreamReader {
    /// 读取下一个 NAL 单元
    ///
    /// 解析逻辑:
    /// 1. 逐字节读取数据到缓冲区
    /// 2. 检测 00 00 01 起始码
    /// 3. 第一个起始码标记 NAL 开始
    /// 4. 第二个起始码标记 NAL 结束
    /// 5. 提取中间的 NAL 数据并返回
    pub async fn read_frame(&mut self, _with_meta: bool) -> Result<Option<VideoFrame>> {
        loop {
            // 逐字节读取
            let mut byte = [0u8; 1];
            self.stream.read_exact(&mut byte).await?;
            self.buffer.extend_from_slice(&byte);

            // 检查 3-byte 起始码 00 00 01
            let buf_len = self.buffer.len();
            if buf_len >= 3 {
                let last_3 = &self.buffer[buf_len - 3..];

                if last_3 == [0x00, 0x00, 0x01] {
                    if self.first_start_code_pos.is_none() {
                        // 记录第一个起始码位置
                        self.first_start_code_pos = Some(buf_len - 3);
                    } else {
                        // 提取 NAL 单元
                        let start = self.first_start_code_pos.unwrap() + 3;
                        let end = buf_len - 3;
                        let nal_data = self.buffer[start..end].to_vec();

                        // 解析 NAL 类型
                        let nal_type = nal_data[0] & 0x1F;
                        let frame_type = match nal_type {
                            7 | 8 => FrameType::Config,  // SPS/PPS
                            _ => FrameType::Video,
                        };

                        return Ok(Some(VideoFrame::new(0, frame_type, Bytes::from(nal_data))));
                    }
                }
            }
        }
    }
}
```

### 6.3 NAL 单元解析流程图

```
输入数据流:
... 00 00 01 [SPS数据] 00 00 01 [PPS数据] 00 00 01 [IDR数据] 00 00 01 ...
    ↑        ↑         ↑        ↑         ↑        ↑         ↑
    │        │         │        │         │        │         │
    │        │         │        │         │        │         │
    起始码1   NAL1结束   起始码2   NAL2结束   起始码3   NAL3结束   起始码4
             NAL1提取            NAL2提取            NAL3提取

解析状态机:
┌─────────────┐    检测到 00 00 01    ┌─────────────┐
│  等待第一个   │─────────────────────►│  等待第二个  │
│   起始码     │                      │   起始码     │
└─────────────┘                      └──────┬──────┘
                                             │
                                             │ 检测到 00 00 01
                                             ▼
                                      ┌─────────────┐
                                      │  提取NAL     │
                                      │  返回帧      │
                                      └──────┬──────┘
                                             │
                                             │ 循环
                                             ▼
                                      ┌─────────────┐
                                      │  等待下一个   │
                                      │   起始码     │
                                      └─────────────┘
```

### 6.4 SPS 解析获取分辨率

```rust
// src/main.rs - SPS 解析器
struct BitReader<'a> {
    data: &'a [u8],
    byte_offset: usize,
    bit_offset: u8,
}

impl BitReader {
    /// 读取 Exp-Golomb 编码的无符号整数 (ue(v))
    fn read_ue(&mut self) -> Option<u32>;

    /// 读取 Exp-Golomb 编码的有符号整数 (se(v))
    fn read_se(&mut self) -> Option<i32>;
}

fn parse_sps_resolution(sps_data: &[u8]) -> Option<(u32, u32)> {
    // SPS 结构 (简化):
    // - NAL header (1 byte)
    // - profile_idc (8 bits)
    // - constraint_flags (8 bits)
    // - level_idc (8 bits)
    // - seq_parameter_set_id (ue(v))
    // - [High Profile specific data]
    // - log2_max_frame_num_minus4 (ue(v))
    // - pic_order_cnt_type (ue(v))
    // - ...
    // - pic_width_in_mbs_minus1 (ue(v))      ← 宽度
    // - pic_height_in_map_units_minus1 (ue(v)) ← 高度
    // - frame_mbs_only_flag (1 bit)
    // - frame_cropping_flag (1 bit)
    // - [cropping offsets]

    // 计算分辨率:
    // width = (pic_width_in_mbs_minus1 + 1) * 16 - crop_left - crop_right
    // height = (pic_height_in_map_units_minus1 + 1) * 16 * (2 - frame_mbs_only_flag)
    //          - crop_top - crop_bottom
}
```

---

## 7. 控制流处理

### 7.1 控制消息类型

```rust
// src/scrcpy/control.rs
#[repr(u8)]
pub enum ControlMessageType {
    InjectKeycode = 0,            // 按键事件
    InjectText = 1,               // 文本输入
    InjectTouch = 2,              // 触摸事件
    InjectScroll = 3,             // 滚动事件
    SetScreenPowerMode = 4,       // 屏幕电源控制
    ExpandNotificationPanel = 5,  // 展开通知栏
    CollapseNotificationPanel = 6,// 收起通知栏
    GetClipboard = 7,             // 获取剪贴板
    SetClipboard = 9,             // 设置剪贴板（scrcpy 3.x）
    RotateDevice = 10,            // 旋转设备
    // ... 更多类型可以翻翻源码
}
```

### 7.2 触摸事件协议 (32 字节)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Touch Event Message Format (32 bytes)                │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field         │ Type      │ Description                │
│  ───────┼──────┼───────────────┼───────────┼─────────────────────────── │
│    0    │  1   │ type          │ u8        │ = 2 (InjectTouch)          │
│    1    │  1   │ action        │ u8        │ 0=Down, 1=Up, 2=Move       │
│    2    │  8   │ pointer_id    │ i64 BE    │ -1=鼠标, >=0=触摸           │
│   10    │  4   │ x             │ u32 BE    │ 像素坐标                    │
│   14    │  4   │ y             │ u32 BE    │ 像素坐标                    │
│   18    │  2   │ width         │ u16 BE    │ 屏幕宽度                    │
│   20    │  2   │ height        │ u16 BE    │ 屏幕高度                    │
│   22    │  2   │ pressure      │ u16 BE    │ 0x0000-0xFFFF (0.0-1.0)    │
│   24    │  4   │ action_button │ u32 BE    │ 鼠标=1, 触摸=0              │
│   28    │  4   │ buttons       │ u32 BE    │ 按钮状态                    │
│  ───────┴──────┴───────────────┴───────────┴─────────────────────────── │
│                                                                         │
│  示例 (鼠标点击 DOWN):                                                    │
│  [02, 00, ff, ff, ff, ff, ff, ff, ff, ff,    ← type=2, action=0, id=-1  │
│   00, 00, 01, 2c, 00, 00, 02, 58,            ← x=300, y=600             │
│   04, 38, 07, 80,                            ← width=1080, height=1920  │
│   ff, ff,                                    ← pressure=1.0             │
│   00, 00, 00, 01,                            ← action_button=1          │
│   00, 00, 00, 01]                            ← buttons=1                │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.3 触摸动作枚举

```rust
#[repr(u8)]
pub enum AndroidMotionEventAction {
    Down = 0,        // ACTION_DOWN - 第一个手指按下
    Up = 1,          // ACTION_UP - 最后一个手指抬起
    Move = 2,        // ACTION_MOVE - 手指移动
    Cancel = 3,      // ACTION_CANCEL - 事件取消
    PointerDown = 5, // ACTION_POINTER_DOWN - 非第一个手指按下
    PointerUp = 6,   // ACTION_POINTER_UP - 非最后一个手指抬起
    HoverMove = 7,   // ACTION_HOVER_MOVE - 鼠标悬停移动
    HoverEnter = 9,  // ACTION_HOVER_ENTER - 鼠标进入
    HoverExit = 10,  // ACTION_HOVER_EXIT - 鼠标离开
}
```

### 7.4 按键事件协议 (14 字节)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Key Event Message Format (14 bytes)                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field     │ Type    │ Description                      │
│  ───────┼──────┼───────────┼─────────┼───────────────────────────────── │
│    0    │  1   │ type      │ u8      │ = 0 (InjectKeycode)              │
│    1    │  1   │ action    │ u8      │ 0=Down, 1=Up                     │
│    2    │  4   │ keycode   │ u32 BE  │ Android KeyCode                  │
│    6    │  4   │ repeat    │ u32 BE  │ 重复次数                          │
│   10    │  4   │ metastate │ u32 BE  │ 修饰键状态 (Shift/Ctrl/Alt)        │
│                                                                         │
│  常用 Android KeyCode:                                                   │
│    KEYCODE_BACK = 4       ← 返回键                                       │
│    KEYCODE_HOME = 3       ← Home键                                      │
│    KEYCODE_VOLUME_UP = 24                                               │
│    KEYCODE_VOLUME_DOWN = 25                                             │
│    KEYCODE_POWER = 26                                                   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.5 滚动事件协议 (21 字节)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                   Scroll Event Message Format (21 bytes)                │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field   │ Type    │ Description                        │
│  ───────┼──────┼─────────┼─────────┼─────────────────────────────────── │
│    0    │  1   │ type    │ u8      │ = 3 (InjectScroll)                 │
│    1    │  4   │ x       │ i32 BE  │ 像素坐标                            │
│    5    │  4   │ y       │ i32 BE  │ 像素坐标                            │
│    9    │  2   │ width   │ u16 BE  │ 屏幕宽度                            │
│   11    │  2   │ height  │ u16 BE  │ 屏幕高度                            │
│   13    │  2   │ hscroll │ i16 BE  │ 水平滚动量 (×2048)                  │
│   15    │  2   │ vscroll │ i16 BE  │ 垂直滚动量 (×2048)                  │
│   17    │  4   │ buttons │ u32 BE  │ 按钮状态                            │
│                                                                         │
│  滚动值编码 (根据官方 scrcpy 抓包分析):                                     │
│    向下滚动: vscroll = -2048 (0xf800)                                    │
│    向上滚动: vscroll = +2048 (0x0800)                                    │
│    前端传入 -1/0/1，后端需要乘以 2048 转换                                  │
│                                                                         │
│  示例 (向下滚动):                                                         │
│  [03, 00, 00, 04, 27, 00, 00, 03, 70,    ← type=3, x=1063, y=880        │
│   05, a0, 0b, 90,                        ← width=1440, height=2960      │
│   00, 00, f8, 00,                        ← hscroll=0, vscroll=-2048     │
│   00, 00, 00, 00]                        ← buttons=0                    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.6 ControlChannel 实现

```rust
// src/scrcpy/control.rs
pub struct ControlChannel {
    stream: TcpStream,  // TCP 连接到 scrcpy-server
}

impl ControlChannel {
    /// 发送触摸事件
    pub async fn send_touch_event(&mut self, event: &TouchEvent) -> Result<()> {
        let mut msg = Vec::with_capacity(32);

        // 构建消息 (所有多字节字段使用大端序)
        msg.push(ControlMessageType::InjectTouch as u8);  // type
        msg.push(event.action as u8);                      // action
        msg.extend_from_slice(&event.pointer_id.to_be_bytes());  // pointer_id (i64)

        // 计算像素坐标 (归一化坐标 × 屏幕尺寸)
        let x_fixed = (event.x * event.width as f32) as u32;
        let y_fixed = (event.y * event.height as f32) as u32;
        msg.extend_from_slice(&x_fixed.to_be_bytes());
        msg.extend_from_slice(&y_fixed.to_be_bytes());

        msg.extend_from_slice(&(event.width as u16).to_be_bytes());
        msg.extend_from_slice(&(event.height as u16).to_be_bytes());

        // pressure: 16位定点数 (0.0 → 0x0000, 1.0 → 0xFFFF)
        let pressure_u16 = (event.pressure * 0xFFFF as f32) as u16;
        msg.extend_from_slice(&pressure_u16.to_be_bytes());

        // action_button 和 buttons 的特殊处理
        let action_button = if event.pointer_id == -1 { 1u32 } else { 0u32 };
        let buttons = match event.action {
            Up | PointerUp => 0u32,
            _ => event.buttons,
        };
        msg.extend_from_slice(&action_button.to_be_bytes());
        msg.extend_from_slice(&buttons.to_be_bytes());

        // 发送并刷新
        self.stream.write_all(&msg).await?;
        self.stream.flush().await?;

        Ok(())
    }
}
```

### 7.7 文本注入协议

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Text Inject Message Format                           │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field   │ Type      │ Description                      │
│  ───────┼──────┼─────────┼───────────┼───────────────────────────────── │
│    0    │  1   │ type    │ u8        │ = 1 (InjectText)                 │
│    1    │  4   │ length  │ u32 BE    │ 文本字节长度                      │
│    5    │  N   │ text    │ [u8; N]   │ UTF-8 编码的文本内容              │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.8 剪贴板设置协议

```
┌─────────────────────────────────────────────────────────────────────────┐
│                   Set Clipboard Message Format                          │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field    │ Type      │ Description                     │
│  ───────┼──────┼──────────┼───────────┼──────────────────────────────── │
│    0    │  1   │ type     │ u8        │ = 9 (SetClipboard)              │
│    1    │  8   │ sequence │ u64 BE    │ 同步序列号 (通常为0)             │
│    9    │  1   │ paste    │ u8        │ 0=仅设置, 1=设置并粘贴           │
│   10    │  4   │ length   │ u32 BE    │ 文本字节长度                     │
│   14    │  N   │ text     │ [u8; N]   │ UTF-8 编码的文本内容             │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.9 统一控制事件类型

为了支持多种控制事件，使用统一的枚举类型：

```rust
// src/scrcpy/control.rs

// 文本输入事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEvent {
    pub text: String,
}

// 剪贴板事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEvent {
    pub text: String,
    pub paste: bool,  // 是否同时模拟粘贴操作
}

// 统一的控制事件类型
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlEvent {
    #[serde(rename = "touch")]
    Touch(TouchEvent),
    #[serde(rename = "key")]
    Key(KeyEvent),
    #[serde(rename = "text")]
    Text(TextEvent),
    #[serde(rename = "clipboard")]
    Clipboard(ClipboardEvent),
    #[serde(rename = "scroll")]
    Scroll(ScrollEvent),
}
```

### 7.10 键盘输入支持

当前实现不是 Web JS 键盘映射，而是 **Flutter -> Rust Session -> scrcpy 控制通道**。

输入策略是：

- 优先 UHID（实体键盘语义，适配中文输入法链路）
- UHID 不支持的 keycode 自动回退 Inject
- UHID 在会话建立时预热，减少首键延迟

输入架构图（中文）：

```text
┌──────────────────────┐
│ Flutter 键盘事件      │
└──────────┬───────────┘
           │
           ▼
┌──────────────────────┐
│ gh_api::send_key     │
└──────────┬───────────┘
           │
           ▼
┌──────────────────────┐
│ Session::send_key    │
│ 模式: Inject/Uhid/Auto│
└───────┬────────┬─────┘
        │        │
        ▼        ▼
┌──────────────┐ ┌─────────────────┐
│ UHID 路径     │ │ Inject 注入路径  │
└───────┬──────┘ └────────┬────────┘
        │                  │
        └────────┬─────────┘
                 ▼
          ┌──────────────┐
          │ Android 输入法 │
          │ 与焦点控件链路 │
          └──────────────┘
```

输入决策流程（中文）：

```text
[收到按键]
    |
    v
[Session::send_key]
    |
    +--> [模式=Inject] -------------> [发送 InjectKeycode] ---> [结束]
    |
    +--> [模式=Uhid/Auto]
              |
              v
   [supports_android_keycode ?]
              |
         +----+-----+
         |          |
        是          否
         |          |
         v          v
[确保 UHID 已创建] [回退 InjectKeycode]
         |
         v
[更新 report 并发送 UhidInput]
         |
         v
       [结束]
```

### 7.11 剪贴板粘贴功能

当前实现已升级为 scrcpy 协议语义，支持双向同步。

- Windows -> Android：`set_clipboard(session_id, text, paste)`  
- Android -> Windows：通过控制通道 device message 回推，再由 Flutter 写入 Windows 剪贴板

剪贴板架构图（中文）：

```text
┌──────────────────────┐      ┌─────────────────────────┐
│ Flutter 剪贴板接口    │<-----│ Runtime ClipboardChanged │
└──────────┬───────────┘      └──────────┬──────────────┘
           │                              ▲
           ▼                              │
┌──────────────────────┐      ┌──────────┴──────────────┐
│ gh_api::set_clipboard│----->│ Session / ControlChannel │
└──────────────────────┘      └──────────┬──────────────┘
                                          │
                                          ▼
                                ┌────────────────────────┐
                                │ Android scrcpy-server   │
                                └────────────────────────┘
```

双向流程图（中文）：

```text
流程 A：Windows -> Android

[用户触发粘贴/同步]
          |
          v
[读取 Windows 剪贴板]
          |
          v
[set_clipboard(text, paste=true/false)]
          |
          v
[设备剪贴板已更新]


流程 B：Android -> Windows

[设备侧剪贴板变化]
          |
          v
[收到 device message(type=0)]
          |
          v
[Session 缓存 clipboard_updates]
          |
          v
[Runtime 发送 ClipboardChanged]
          |
          v
[Flutter 写入 Windows 剪贴板]
```

ACK 机制说明：

- `paste=false`：发送 `SetClipboard(type=9)` 时带序列号并等待 ACK（默认 500ms）
- `paste=true`：立即发送，不等待 ACK，用于低延迟直接粘贴

### 7.12 鼠标滚轮支持

前端监听滚轮事件，将滚动量发送到后端：

```javascript
// 滚动事件结构
{
    type: 'scroll',
    x: 0.5,           // 归一化坐标 [0, 1]
    y: 0.3,           // 归一化坐标 [0, 1]
    width: 1920,      // 视频宽度
    height: 1080,     // 视频高度
    hscroll: 0,       // 水平滚动量 (-1, 0, 1)
    vscroll: -1       // 垂直滚动量 (-1=向下, 1=向上)
}

// 滚轮事件处理
function handleWheel(e) {
    e.preventDefault();

    const coords = normalizeCoords(e.clientX, e.clientY);

    // deltaY > 0 表示向下滚动，对应 vscroll < 0
    const vscroll = e.deltaY > 0 ? -1 : (e.deltaY < 0 ? 1 : 0);
    const hscroll = e.deltaX > 0 ? -1 : (e.deltaX < 0 ? 1 : 0);

    if (vscroll !== 0 || hscroll !== 0) {
        sendScrollEvent(coords.x, coords.y, hscroll, vscroll);
    }
}

canvas.addEventListener('wheel', handleWheel, { passive: false });
```

后端 ScrollEvent 结构：

```rust
// src/scrcpy/control.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollEvent {
    pub x: f32,           // 归一化坐标 [0, 1]
    pub y: f32,           // 归一化坐标 [0, 1]
    pub width: u32,       // 视频宽度
    pub height: u32,      // 视频高度
    pub hscroll: i32,     // 水平滚动量
    pub vscroll: i32,     // 垂直滚动量
}
```

---

## 7.5 屏幕旋转自动适配

### 7.5.1 功能概述

当前实现在运行时处理分辨率/方向变化，并通过会话事件通知上层更新渲染与触控映射。

### 7.5.2 后端检测机制

```rust
// runtime.rs（简化）
if (width, height) != last_size || frame_generation != active_generation {
    active_generation = frame_generation;
    Self::push_event(
        &session_id,
        &events,
        SessionEvent::ResolutionChanged {
            width,
            height,
            new_handle: handle,
            generation: frame_generation,
        },
    );
    last_size = (width, height);
}
```

### 7.5.3 事件结构

```rust
pub enum SessionEvent {
    // ...
    ResolutionChanged {
        width: u32,
        height: u32,
        new_handle: i64,
        generation: u64,
    },
}
```

### 7.5.4 上层处理建议

- 收到 `ResolutionChanged` 后立即更新渲染目标尺寸。
- 触控映射使用当前视频尺寸，避免旋转后坐标偏移。
- 代际（generation）变化时丢弃旧帧，优先渲染新代际最新帧。

### 7.5.5 配置变化广播机制

当前实现已从旧配置广播机制调整为 **会话事件回调**：

- Rust runtime 在分辨率/方向变化时产出 `SessionEvent::ResolutionChanged`。
- 事件通过 `flutter_callback_register::notify_session_event(...)` 推送给上层。
- 上层收到事件后更新渲染尺寸与触控映射基准。

```rust
// runtime.rs（简化）
Self::push_event(
    &session_id,
    &events,
    SessionEvent::ResolutionChanged {
        width,
        height,
        new_handle,
        generation,
    },
);

flutter_callback_register::notify_session_event(session_id, &payload);
```

## 9. 解码与渲染链路（当前实现）

### 9.1 解码模式

`SessionConfigV2.decoder_mode` 支持：

- `PreferHardware`：优先硬解，失败回退软解
- `ForceHardware`：强制硬解
- `ForceSoftware`：强制软解

### 9.2 解码流程

1. `FramedVideoStreamReader::read_packet()` 读取 scrcpy 分帧包
2. `DecoderPipeline::push_framed_packet(...)` 投递解码队列
3. `FfmpegDecoder` 产出 `DecodedFrame::{GpuShared,CpuBgra}`
4. runtime 通过回调桥将帧与会话事件上报上层

### 9.3 渲染输出模式

- `RenderPipelineMode::Original`：共享句柄路径（GPU）
- `RenderPipelineMode::CpuPixelBufferV2`：RGBA 像素缓冲路径（CPU）

### 9.4 低延迟策略

- 解码/帧队列使用小容量，优先最新帧
- 同代际帧覆盖，减少历史帧积压
- 旋转重配时以 `generation` 隔离旧帧

---
## 10. 数据流转图

### 10.1 视频流数据流转（当前实现）

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        视频流数据流转（FFI 主链路）                      │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Android 设备                                                            │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  Surface -> MediaCodec(H.264) -> scrcpy-server v3.3.4          │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     │ video socket + framed packet      │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ adb forward: tcp:video_port -> localabstract:scrcpy            │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│  PC / Rust Runtime                  ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ SessionManager::connect_v2()                                   │    │
│  │ - ScrcpyServer::connect_video()                                │    │
│  │ - read_video_header(dummy + codec_meta)                        │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ FramedVideoStreamReader                                        │    │
│  │ - read_packet() 读取完整编码包                                 │    │
│  │ - is_config / is_keyframe 标记同步到解码管线                    │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ DecoderPipeline + FfmpegDecoder                                │    │
│  │ - PreferHardware / ForceHardware / ForceSoftware               │    │
│  │ - 输出 GpuShared 或 CpuBgra 帧                                  │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ runtime 回调桥接                                                 │    │
│  │ - notify_v1_frame(handle, w, h, gen, pts)                      │    │
│  │ - notify_v2_frame_raw(frame_id, rgba, w, h, ...)               │    │
│  │ - SessionEvent::ResolutionChanged / Running / Reconnecting      │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ Upper App (FFI caller)                                         │    │
│  │ - 消费帧通知与会话事件，完成渲染与状态管理                         │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 10.2 控制流数据流转（触控/按键）

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         控制流数据流转（当前实现）                       │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Upper App                                                              │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ send_touch / send_key / send_scroll / send_text / clipboard     │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ scrcpy::scrcpy_service                                          │    │
│  │ - 根据 session_id 定位 runtime                                   │    │
│  │ - 转发到 SessionRuntime::send_*                                  │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ RealSessionRuntime 命令队列                                      │    │
│  │ - RuntimeCommand::{Touch,Key,Scroll,Text,Clipboard,...}         │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ ControlChannel                                                  │    │
│  │ - 按 scrcpy 控制协议编码（二进制 Big Endian）                      │    │
│  │ - write_all/flush 到 control socket                             │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ adb forward: tcp:control_port -> localabstract:scrcpy           │    │
│  └──────────────────────────────────┬──────────────────────────────┘    │
│                                     │                                   │
│                                     ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ scrcpy-server                                                   │    │
│  │ - 解析控制包                                                     │    │
│  │ - 注入到 Android InputManager                                    │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 11. 关键技术细节

### 11.1 字节序处理

scrcpy 协议使用 **大端序 (Big Endian)**：

```rust
// Rust 中的大端序转换
let x: u32 = 300;
let bytes = x.to_be_bytes();  // [0x00, 0x00, 0x01, 0x2C]

let pointer_id: i64 = -1;
let bytes = pointer_id.to_be_bytes();  // [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
```

### 11.2 压力值与滚动值编码

**压力值**使用 16 位定点数：

```rust
// float [0.0, 1.0] → u16 [0x0000, 0xFFFF]
let pressure: f32 = 1.0;
let pressure_u16 = (pressure * 0xFFFF as f32) as u16;  // 0xFFFF

let pressure: f32 = 0.0;
let pressure_u16 = (pressure * 0xFFFF as f32) as u16;  // 0x0000

let pressure: f32 = 0.5;
let pressure_u16 = (pressure * 0xFFFF as f32) as u16;  // 0x7FFF
```

**滚动值**使用 2048 作为单位（根据官方 scrcpy 抓包分析）：

```rust
// 前端传入 -1, 0, 1 → 后端乘以 2048
// 向下滚动: -1 * 2048 = -2048 (0xf800)
// 向上滚动:  1 * 2048 =  2048 (0x0800)

let vscroll: i32 = -1;  // 向下滚动
let vscroll_i16 = (vscroll * 2048) as i16;  // -2048 = 0xf800

let vscroll: i32 = 1;   // 向上滚动
let vscroll_i16 = (vscroll * 2048) as i16;  // 2048 = 0x0800
```

### 11.3 坐标系转换（当前触控链路）

当前实现中，坐标由上层应用计算后通过 `send_touch` 传入 Rust：

- 上层输入坐标：窗口/控件坐标（例如 `clientX/clientY`）
- 归一化坐标：`x_norm/y_norm`，范围 `[0,1]`
- 注入基准尺寸：当前视频帧尺寸（`width/height`，来自当前会话分辨率）

```text
上层坐标 -> 归一化坐标 -> TouchEvent(width/height + x/y) -> scrcpy 注入 

x_norm = (x - view_left) / view_width
y_norm = (y - view_top)  / view_height

x_norm, y_norm 约束到 [0, 1]
TouchEvent.width/height 使用当前视频帧尺寸（避免旋转/降采样后坐标失配）
```

示例：

- 视图区域：`1000x1000`
- 触点：`(300, 200)`
- 当前视频帧：`1080x1920`

则：

- `x_norm = 300 / 1000 = 0.3`
- `y_norm = 200 / 1000 = 0.2`
- 发送 `TouchEvent { x: 0.3, y: 0.2, width: 1080, height: 1920, ... }`

这样可保证设备旋转、分辨率变化、缩放场景下触控仍映射到正确位置。

### 11.4 IDR 帧请求机制（当前实现）

当前实现包含两条 IDR 触发路径：

1. **手动请求**：上层调用 `request_idr(session_id)`，runtime 下发 `RuntimeCommand::RequestIdr`，通过控制通道触发 `send_reset_video()`。
2. **自动恢复**：解码管线出现失败信号时，runtime 自动请求 IDR；硬解模式会进入短暂恢复窗口，超时则升级为会话重连。

```
IDR 触发与恢复流程

Upper App             scrcpy_service/core_runtime            scrcpy-server
   │                             │                                │
   │ request_idr()               │                                │
   │────────────────────────────>│ RuntimeCommand::RequestIdr     │
   │                             │───────────────────────────────>│
   │                             │    send_reset_video()          │
   │                             │                                │
   │                             │ <视频关键帧返回>                 │
   │                             │                                │

自动路径（解码失败）

DecoderPipeline fail signal -> runtime 检测 need_idr_signals
                         -> send_reset_video()
                         -> 若硬解: 进入恢复窗口等待 resync
                         -> 成功: 继续当前会话
                         -> 超时: SessionEvent::Reconnecting
```

设计目标：

- 降低花屏/绿屏后的恢复时间。
- 尽量先做“同会话内重同步”，减少整会话重连次数。

### 11.5 SPS/PPS 缓存策略

```rust
// 主循环中的 SPS/PPS 缓存
if frame.frame_type == FrameType::Config {
    let nal_type = frame.data[0] & 0x1F;

    if nal_type == 7 && !sps_cached {
        // SPS - 同时解析分辨率
        let mut nal_with_start_code = vec![0x00, 0x00, 0x00, 0x01];
        nal_with_start_code.extend_from_slice(&frame.data);

        let mut config = video_config.write().await;
        config.sps = Some(Bytes::from(nal_with_start_code));

        // 解析分辨率
        if let Some((w, h)) = parse_sps_resolution(&frame.data) {
            config.width = w;
            config.height = h;
        }
        sps_cached = true;
    } else if nal_type == 8 && !pps_cached {
        // PPS
        let mut nal_with_start_code = vec![0x00, 0x00, 0x00, 0x01];
        nal_with_start_code.extend_from_slice(&frame.data);

        let mut config = video_config.write().await;
        config.pps = Some(Bytes::from(nal_with_start_code));
        pps_cached = true;
    }
}
```

---

## 12. 配置参数说明

### 12.1 会话配置参数（当前 API）

| 字段 | 说明 |
| --- | --- |
| `adb_path` | ADB 可执行文件路径 |
| `server_path` | scrcpy-server-v3.3.4 路径 |
| `device_id` | 目标设备序列号 |
| `max_size` | 最大分辨率（长边），0=不限制 |
| `bit_rate` | 视频码率（bps） |
| `max_fps` | 最大帧率，0=不限制 |
| `video_port` | 视频端口（自动探测可用端口） |
| `control_port` | 控制端口（自动探测可用端口） |
| `video_encoder` | 指定编码器（可选） |
| `intra_refresh_period` | IDR 间隔（秒） |
| `turn_screen_off` | 启动后是否关屏 |
| `stay_awake` | 会话期间是否常亮 |
| `scrcpy_verbosity` | scrcpy 日志级别 |

V2 额外参数：

- `decoder_mode`
- `render_pipeline_mode`

### 12.2 性能调优建议

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          性能调优参数建议                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  场景              │ max_size │ bit_rate  │ max_fps │ idr_interval       │
│  ─────────────────┼──────────┼───────────┼─────────┼─────────────────── │
│  高画质            │  1920    │ 8000000   │   60    │      2             │
│  平衡模式          │  1280    │ 4000000   │   60    │      1             │
│  低延迟            │  1280    │ 2000000   │   60    │      1             │
│  流量节省          │   720    │ 1000000   │   30    │      2             │
│                                                                         │
│  说明:                                                                   │
│  - max_size 越小,编码越快,延迟越低                                         │
│  - bit_rate 越高,画质越好,带宽占用越大                                      │
│  - idr_interval 越小,新客户端启动越快,但压缩效率降低                          │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 12.3 模拟器兼容性说明

部分 x86 模拟器在 H.264 编码能力上弱于真机，常见表现为高延迟和掉帧。

**建议参数：**

1. `max_size=1080`, `bit_rate=2_000_000`
2. 仍不稳定时降到 `max_size=720`, `bit_rate=1_000_000`
3. 必要时指定 `video_encoder`
4. 降低 `max_fps` 以换取稳定性

**说明：**

- 当前链路使用 FFmpeg 解码，不依赖浏览器解码器。
- 参数优先目标是“稳定 + 低延迟”，而非极限画质。

### 12.4 x86 模拟器适配技术细节（当前实现）

#### 12.4.1 SPS 防竞争字节处理

```rust
fn remove_emulation_prevention_bytes(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x03 {
            result.push(0x00);
            result.push(0x00);
            i += 3;
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}
```

#### 12.4.2 IDR 恢复机制

- 手动：`request_idr(session_id)`
- 自动：解码失败信号触发 `send_reset_video()`
- 硬解模式：先尝试短窗口重同步，超时再重连

#### 12.4.3 视频流批量读取优化

视频读取采用批量缓冲，减少逐字节读取开销。

#### 12.4.4 修改汇总（当前代码）

| 文件 | 修改内容 |
|------|----------|
| `src/scrcpy/runtime/scrcpy_core_runtime.rs` | 会话运行时、事件回调、重连与 IDR 恢复 |
| `src/session/manager.rs` | connect_v2 建链与方向控制 |
| `src/scrcpy/server.rs` | server 参数、端口探测、协议头读取 |
| `src/decoder/ffmpeg_decoder.rs` | FFmpeg 解码器（硬解/软解策略） |
| `src/decoder/pipeline.rs` | 解码管线与回压控制 |
| `src/utils/port.rs` | 端口可用性检测与自动寻找 |


## 13. 错误处理

### 13.1 错误类型定义

```rust
// src/error.rs
#[derive(Error, Debug)]
pub enum ScrcpyError {
    #[error("ADB error: {0}")]
    Adb(String),              // ADB 命令执行失败

    #[error("Device not found")]
    DeviceNotFound,           // 设备未找到

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),  // IO 错误

    #[error("Network error: {0}")]
    Network(String),          // 网络连接错误

    #[error("Video stream error: {0}")]
    VideoStream(String),      // 视频流解析错误

    #[error("Parse error: {0}")]
    Parse(String),            // 数据解析错误
}

pub type Result<T> = std::result::Result<T, ScrcpyError>;
```

### 13.2 常见错误及解决方案

| 错误                                 | 原因                 | 解决方案                     |
| ------------------------------------ | -------------------- | ---------------------------- |
| `ADB not found`                      | ADB 路径错误         | 检查 `--adb-path` 参数       |
| `No devices connected`               | 设备未连接           | 检查 USB 连接或 WiFi 调试    |
| `Server file not found`              | server JAR 不存在    | 检查 `--server-path` 参数    |
| `Failed to connect after 5 attempts` | 端口转发失败         | 重启 ADB 服务                |
| `Buffer overflow`                    | 视频流积压           | 提高处理速度或降低画质       |
| `No available port found`            | 端口范围内无可用端口 | 释放占用的端口或调整起始端口 |

---

## 14. 端口自动寻找机制

### 14.1 功能概述

当指定的端口被占用时，程序会自动向后搜索可用端口，无需手动修改配置。这在以下场景特别有用：

- 同时运行多个 rust-scrcpy 实例
- 端口被其他程序占用
- 之前的实例未正确释放端口

### 14.2 端口工具模块

```rust
// src/utils/port.rs

/// 检查端口是否可用
pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// 从指定端口开始，寻找第一个可用端口
///
/// # Arguments
/// * `start_port` - 起始端口
/// * `max_attempts` - 最大尝试次数（向后搜索的范围）
///
/// # Returns
/// * `Ok(port)` - 找到的可用端口
/// * `Err` - 在范围内未找到可用端口
pub fn find_available_port(start_port: u16, max_attempts: u16) -> Result<u16> {
    let end_port = start_port.saturating_add(max_attempts);

    for port in start_port..=end_port {
        if is_port_available(port) {
            if port != start_port {
                info!("📌 Port {} is occupied, using port {} instead", start_port, port);
            }
            return Ok(port);
        }
    }

    Err(ScrcpyError::NoAvailablePort(start_port, end_port))
}

/// 寻找多个连续可用端口
pub fn find_available_ports(start_port: u16, count: usize, max_attempts: u16) -> Result<Vec<u16>>;
```

### 14.3 端口配置说明

| 端口类型       | 默认值 | 搜索范围        | 用途          |
| -------------- | ------ | --------------- | ------------- |
| Video Port     | 27183  | 27183-27283     | scrcpy 视频流 |
| Control Port   | 27184  | video_port+1 起 | scrcpy 控制流 |

### 14.4 工作流程

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        端口自动寻找流程                                   │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  1. 用户配置端口                                                          │
│     --video-port 27183 --control-port 27184              │
│                                                                         │
│  2. 检测视频端口                                                          │
│     ┌─────────────────────────────────────────────────────────┐         │
│     │  27183 被占用? ──Yes──> 27184 被占用? ──Yes──> 27185...   │         │
│     │       │                      │                          │         │
│     │      No                     No                          │         │
│     │       ▼                      ▼                          │         │
│     │  使用 27183              使用 27184                      │         │
│     └─────────────────────────────────────────────────────────┘         │
│                                                                         │
│  3. 检测控制端口 (从 video_port + 1 开始)                                  │
│     确保控制端口 > 视频端口，避免冲突                                        │
│                                                                         │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 14.5 实际运行示例

```
2025-12-26T11:02:05.703681Z  INFO rust_scrcpy::utils::port: 📌 Port 27183 is occupied, using port 27185 instead
2025-12-26T11:02:05.745795Z  INFO rust_scrcpy::scrcpy::server: 🚀 Starting scrcpy-server...
2025-12-26T11:02:05.745892Z  INFO rust_scrcpy::scrcpy::server:    Video port: 27185 (requested: 27183)
2025-12-26T11:02:05.745969Z  INFO rust_scrcpy::scrcpy::server:    Control port: 27186 (requested: 27184)
```

### 14.6 ScrcpyServer 端口管理

```rust
// src/scrcpy/server.rs

pub struct ScrcpyServer {
    video_port: u16,           // 用户请求的视频端口
    actual_video_port: u16,    // 实际使用的视频端口
    control_port: u16,         // 用户请求的控制端口
    actual_control_port: u16,  // 实际使用的控制端口
    // ...
}

impl ScrcpyServer {
    pub fn with_config(...) -> Result<Self> {
        // 自动寻找可用端口
        let actual_video_port = find_available_port(video_port, 100)?;

        // 控制端口从视频端口+1开始搜索，避免冲突
        let actual_control_port = find_available_port(
            if control_port <= actual_video_port {
                actual_video_port + 1
            } else {
                control_port
            },
            100
        )?;

        Ok(Self { ... })
    }

    /// 获取实际使用的视频端口
    pub fn get_actual_video_port(&self) -> u16 {
        self.actual_video_port
    }

    /// 获取实际使用的控制端口
    pub fn get_actual_control_port(&self) -> u16 {
        self.actual_control_port
    }
}
```


### 14.8 错误处理

当在搜索范围内找不到可用端口时，会返回 `NoAvailablePort` 错误：

```rust
// src/error.rs

#[derive(Error, Debug)]
pub enum ScrcpyError {
    // ...

    #[error("No available port found in range {0}-{1}")]
    NoAvailablePort(u16, u16),
}
```

---

## 附录

### A. 项目文件结构

```
rust-ws-scrcpy/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── flutter_callback_register.rs
│   ├── frb_generated.rs
│   ├── config/
│   │   ├── config.rs
│   │   └── mod.rs
│   ├── gh_api/
│   │   ├── flutter_api.rs
│   │   └── mod.rs
│   ├── gh_common/
│   │   ├── error.rs
│   │   ├── event.rs
│   │   ├── model.rs
│   │   ├── port.rs
│   │   └── mod.rs
│   ├── scrcpy/
│   │   ├── mod.rs
│   │   ├── scrcpy_service.rs
│   │   ├── client/
│   │   ├── config/
│   │   ├── decode_core/
│   │   ├── input/
│   │   ├── runtime/
│   │   └── session/
├── scripts/
└── docs/
```

### B. 依赖库说明

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| tokio | 1.42 | 异步运行时 |
| serde / serde_json | 1.0 | 数据序列化 |
| bytes | 1.9 | 高效字节缓冲 |
| tracing | 0.1 | 日志系统 |
| thiserror | 2.0 | 错误处理 |
| ffmpeg-next | 8 | H.264 解码 |
| flutter_rust_bridge | 2.11.1 | FFI 桥接 |
| windows | 0.58 | Windows API |

### C. 参考资料

- [scrcpy 官方仓库](https://github.com/Genymobile/scrcpy)
- [Android InputManager](https://developer.android.com/reference/android/hardware/input/InputManager)

### D. 鸣谢

- [Creeeeeeeeeeper/rust-ws-scrcpy](https://github.com/Creeeeeeeeeeper/rust-ws-scrcpy)
- [Claude Opus 4.5](https://claude.ai)
- [ChatGPT 5.2](https://chatgpt.com)
