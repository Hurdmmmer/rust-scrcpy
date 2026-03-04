use crate::adb::AdbClient;
use crate::error::{Result, ScrcpyError};
use crate::utils::find_available_port;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use std::process::Stdio;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tracing::{info, warn};

const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";
const SOCKET_NAME: &str = "scrcpy";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

// scrcpy 3.3.4 的 codec_meta JSON 格式（当前主链路未直接使用，先注释保留）
// #[derive(Debug, serde::Deserialize)]
// struct CodecMeta {
//     codec: String,
//     width: u32,
//     height: u32,
//     #[serde(rename = "csd-0")]
//     csd_0: Option<String>,  // SPS (base64)
//     #[serde(rename = "csd-1")]
//     csd_1: Option<String>,  // PPS (base64)
// }


pub struct ScrcpyServer {
    adb: AdbClient,
    device_id: String,
    server_path: PathBuf,
    video_port: u16,
    actual_video_port: u16,    // 实际使用的视频端口
    control_port: u16,
    actual_control_port: u16,  // 实际使用的控制端口
    max_size: u32,
    bit_rate: u32,
    max_fps: u32,
    intra_refresh_period: u32,  // 强制IDR帧间隔（秒）
    video_encoder: Option<String>,  // 指定视频编码器
    /// 是否启用 scrcpy 分帧协议（raw_stream=false）。
    ///
    /// - `false`（默认）：保持旧链路，直接输出 Annex-B 原始码流；
    /// - `true`：启用 packet meta 头（12 字节），由客户端按包读取解码。
    ///
    /// 设计原因：
    /// raw 流虽然简单，但需要客户端自行处理 NAL/AU 边界，容易在高分辨率、
    /// 多 slice 或网络抖动下出现错包解码。分帧协议由服务端明确给出包边界，
    /// 对生产链路更稳定。
    use_framed_stream: bool,
    server_process: Option<Child>,
}

impl ScrcpyServer {
    // 旧默认构造入口（当前主链路未调用，先注释保留）。
    // pub fn new(adb: AdbClient, device_id: String, server_path: PathBuf) -> Result<Self> {
    //     // 自动寻找可用端口
    //     let actual_video_port = find_available_port(27183, 100)?;
    //     let actual_control_port = find_available_port(actual_video_port + 1, 100)?;
    //
    //     Ok(Self {
    //         adb,
    //         device_id,
    //         server_path,
    //         video_port: 27183,
    //         actual_video_port,
    //         control_port: 27184,
    //         actual_control_port,
    //         max_size: 1920,       // 最大分辨率
    //         bit_rate: 16_000_000, // 16Mbps - 提高码率改善画质
    //         max_fps: 60,
    //         intra_refresh_period: 1,  // 每1秒强制一个IDR帧
    //         video_encoder: None,
    //         use_framed_stream: false,
    //         server_process: None,
    //     })
    // }

    /// 创建带自定义配置的服务器（自动寻找可用端口）
    pub fn with_config(
        adb: AdbClient,
        device_id: String,
        server_path: PathBuf,
        max_size: u32,
        bit_rate: u32,
        max_fps: u32,
        video_port: u16,
        control_port: u16,
        intra_refresh_period: u32,
        video_encoder: Option<String>,
    ) -> Result<Self> {
        // 自动寻找可用端口
        let actual_video_port = find_available_port(video_port, 100)?;
        // 控制端口从视频端口+1开始搜索，避免冲突
        let actual_control_port = find_available_port(
            if control_port <= actual_video_port { actual_video_port + 1 } else { control_port },
            100
        )?;

        Ok(Self {
            adb,
            device_id,
            server_path,
            video_port,
            actual_video_port,
            control_port,
            actual_control_port,
            max_size,
            bit_rate,
            max_fps,
            intra_refresh_period,
            video_encoder,
            use_framed_stream: false,
            server_process: None,
        })
    }

    /// 设置视频流协议模式。
    ///
    /// `enabled=true` 时使用 scrcpy 分帧协议：
    /// - `raw_stream=false`
    /// - `send_frame_meta=true`
    /// - `send_codec_meta=true`
    ///
    /// `enabled=false` 时保持旧 raw 模式：
    /// - `raw_stream=true`
    /// - `send_frame_meta=false`
    /// - `send_codec_meta=false`
    pub fn set_framed_stream_enabled(&mut self, enabled: bool) {
        self.use_framed_stream = enabled;
    }

    /// 查询当前是否启用分帧协议。
    pub fn is_framed_stream_enabled(&self) -> bool {
        self.use_framed_stream
    }

    // /// 获取实际使用的视频端口
    // pub fn get_actual_video_port(&self) -> u16 {
    //     self.actual_video_port
    // }

    // /// 获取实际使用的控制端口
    // pub fn get_actual_control_port(&self) -> u16 {
    //     self.actual_control_port
    // }

    /// 部署服务器到设备
    pub async fn deploy(&self) -> Result<()> {
        info!("📦 Deploying scrcpy-server to device...");

        // 检查本地服务器文件是否存在
        if !self.server_path.exists() {
            return Err(ScrcpyError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Server file not found: {:?}", self.server_path),
            )));
        }

        // 推送服务器到设备
        let local_path = self.server_path.to_str().ok_or_else(|| {
            ScrcpyError::Parse("Invalid server path".to_string())
        })?;

        info!("  Pushing {} to device...", local_path);
        self.adb
            .push(&self.device_id, local_path, DEVICE_SERVER_PATH)
            .await?;

        info!("✅ Server deployed successfully");
        Ok(())
    }

    /// 启动scrcpy-server
    pub async fn start(&mut self) -> Result<()> {
        info!("🚀 Starting scrcpy-server...");
        info!("   Video port: {} (requested: {})", self.actual_video_port, self.video_port);
        info!("   Control port: {} (requested: {})", self.actual_control_port, self.control_port);

        // 设置端口转发 - 视频socket
        info!("  Setting up video port forwarding: localabstract:{}", SOCKET_NAME);
        self.adb
            .forward(
                &self.device_id,
                self.actual_video_port,
                &format!("localabstract:{}", SOCKET_NAME),
            )
            .await?;

        // 设置端口转发 - 控制socket (使用同一个 abstract socket，scrcpy 会区分连接)
        info!("  Setting up control port forwarding: localabstract:{}", SOCKET_NAME);
        self.adb
            .forward(
                &self.device_id,
                self.actual_control_port,
                &format!("localabstract:{}", SOCKET_NAME),
            )
            .await?;

        // 启动server的命令
        // scrcpy 3.x 必须明确指定参数来启用视频流。
        // i-frame-interval 单位是秒，用于控制关键帧恢复速度。
        info!("  IDR frame interval: {}s", self.intra_refresh_period);
        if let Some(ref encoder) = self.video_encoder {
            info!("  Video encoder: {}", encoder);
        }
        let encoder_param = match &self.video_encoder {
            Some(encoder) => format!("video_encoder={} ", encoder),
            None => String::new(),
        };
        let codec_options_param = if self.intra_refresh_period > 0 {
            format!(
                "video_codec_options=i-frame-interval={} ",
                self.intra_refresh_period
            )
        } else {
            String::new()
        };
        let max_size_param = if self.max_size > 0 {
            format!("max_size={} ", self.max_size)
        } else {
            String::new()
        };
        let max_fps_param = if self.max_fps > 0 {
            format!("max_fps={} ", self.max_fps)
        } else {
            String::new()
        };

        let (send_frame_meta, send_codec_meta, raw_stream) = if self.use_framed_stream {
            ("true", "true", "false")
        } else {
            ("false", "false", "true")
        };

        let server_args = format!(
             "CLASSPATH={} app_process / com.genymobile.scrcpy.Server 3.3.4 \
              log_level=info \
              {}\
              video_bit_rate={} \
              {}\
              {}\
              {}\
              tunnel_forward=true \
              send_device_meta=false \
             send_frame_meta={} \
             send_dummy_byte=true \
             send_codec_meta={} \
             raw_stream={} \
             audio=false \
             control=true \
             cleanup=true",
            DEVICE_SERVER_PATH,
            max_size_param,
            self.bit_rate,
            max_fps_param,
            codec_options_param,
            encoder_param,
            send_frame_meta,
            send_codec_meta,
            raw_stream
        );

        info!("  Executing: shell {}", server_args);

        // 使用ADB启动server（异步进程）
        // 注意：可能需要stdin来传递配置
        let adb_path = self.adb.adb_path.clone();
        let device_id = self.device_id.clone();

        let mut command = Command::new(&adb_path);
        command
            .args(&["-s", &device_id, "shell", &server_args])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = command
            .spawn()
            .map_err(|e| ScrcpyError::Adb(format!("Failed to start server: {}", e)))?;

        // 先获取 stderr 用于后台监控
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 { break; }
                    warn!("  Server stderr: {}", line.trim());
                    line.clear();
                }
            });
        }

        // 后台持续读取 stdout，并将首条有效输出作为就绪信号。
        let (ready_tx, mut ready_rx) = tokio::sync::oneshot::channel::<()>();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                let mut ready_tx = Some(ready_tx);

                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    let text = line.trim();
                    if !text.is_empty() {
                        info!("  Server output: {}", text);
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    line.clear();
                }
            });
        } else {
            warn!("  Could not capture server stdout");
        }

        // 启动等待窗口：优先等待首条输出，同时检查进程是否提前退出。
        let wait_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
        let mut readiness_received = false;
        loop {
            if tokio::time::Instant::now() >= wait_deadline {
                warn!("  Timeout waiting for server readiness signal (continue with retries)");
                break;
            }

            if ready_rx.try_recv().is_ok() {
                info!("  Server readiness signal received");
                readiness_received = true;
                break;
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(ScrcpyError::Adb(format!(
                        "scrcpy-server exited unexpectedly before ready: {}",
                        status
                    )));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("  Failed to query server process status: {}", e);
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        self.server_process = Some(child);

        // 生产优化：
        // - 收到 readiness 信号后，只给短缓冲，尽快进入 connect；
        // - 未收到信号（走超时兜底）时，保持较长等待保障稳定性。
        let settle_ms = if readiness_received { 120 } else { 500 };
        info!("  Waiting for server to initialize... {}ms", settle_ms);
        tokio::time::sleep(tokio::time::Duration::from_millis(settle_ms)).await;

        info!("✅ Server started on port {}", self.actual_video_port);
        Ok(())
    }

    /// 连接到scrcpy-server的视频流
    pub async fn connect_video(&self) -> Result<TcpStream> {
        info!("🔌 Connecting to video stream...");

        let addr = format!("127.0.0.1:{}", self.actual_video_port);

        // 尝试连接，带重试机制
        let mut stream = None;
        for attempt in 1..=5 {
            info!("  Connection attempt {}/5...", attempt);
            match TcpStream::connect(&addr).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) if attempt < 5 => {
                    info!("  Connection failed: {}, retrying...", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    return Err(ScrcpyError::Network(format!("Failed to connect after 5 attempts: {}", e)));
                }
            }
        }

        let stream = stream.unwrap();

        // raw_stream=true + control=false 模式：
        // 不需要发送任何 marker，直接连接即可
        // 服务器会发送 dummy byte，然后是 NAL 流

        info!("✅ Connected to video stream");

        Ok(stream)
    }

    /// 连接到scrcpy-server的控制流
    /// 控制流使用独立的端口 (control_port)，通过 adb forward 映射到同一个 abstract socket
    pub async fn connect_control(&self) -> Result<TcpStream> {
        info!("✅ Connecting to control stream...");

        // 使用实际的控制端口
        let addr = format!("127.0.0.1:{}", self.actual_control_port);

        // 连接到控制流
        let stream = TcpStream::connect(&addr).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to connect control: {}", e)))?;

        info!("✅ Connected to control stream on port {}", self.actual_control_port);
        Ok(stream)
    }

    /// 从已连接的 video stream 读取 scrcpy 协议头。
    ///
    /// 参数说明：
    /// - `expect_codec_meta=false`：raw 模式，仅消费 dummy byte；
    /// - `expect_codec_meta=true`：framed 模式，消费 dummy byte + codec meta(12B)。
    ///
    /// 重要：必须由调用方传入正确模式，避免把视频负载误读成协议头。
    pub async fn read_video_header(
        stream: &mut TcpStream,
        expect_codec_meta: bool,
    ) -> Result<()> {
        info!("📖 Reading scrcpy protocol header...");

        // scrcpy 启用 send_dummy_byte=true 时，首字节固定为 dummy byte。

        // 读取 dummy byte (1 byte)
        let mut dummy_byte = [0u8; 1];
        stream.read_exact(&mut dummy_byte).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to read dummy byte: {}", e)))?;
        info!("  Dummy byte: 0x{:02x}", dummy_byte[0]);

        if expect_codec_meta {
            // framed 协议下，dummy 后紧跟 12 字节 codec meta：
            // codec_id(4B BE) + width(4B BE) + height(4B BE)
            let mut meta = [0u8; 12];
            stream
                .read_exact(&mut meta)
                .await
                .map_err(|e| ScrcpyError::Network(format!("Failed to read codec meta: {}", e)))?;

            let codec_id = u32::from_be_bytes([meta[0], meta[1], meta[2], meta[3]]);
            let width = u32::from_be_bytes([meta[4], meta[5], meta[6], meta[7]]);
            let height = u32::from_be_bytes([meta[8], meta[9], meta[10], meta[11]]);

            info!(
                "✅ Protocol header read successfully (framed): codec=0x{:08x}, {}x{}",
                codec_id, width, height
            );

            return Ok(());
        }

        info!("✅ Protocol header read successfully");
        info!("ℹ️  SPS/PPS will be extracted from raw NAL stream");

        Ok(())
    }

    /// 停止服务器
    pub async fn stop(&mut self) -> Result<()> {
        info!("ℹ️ Stopping scrcpy-server...");

        // 杀死server进程
        if let Some(mut child) = self.server_process.take() {
            let _ = child.kill().await;
        }

        // 移除端口转发（使用实际端口）
        let _ = self.adb.forward_remove(&self.device_id, self.actual_video_port).await;
        let _ = self.adb.forward_remove(&self.device_id, self.actual_control_port).await;

        info!("✅ Server stopped");
        Ok(())
    }
}

impl Drop for ScrcpyServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.server_process.take() {
            let _ = child.start_kill();
        }
        // 兜底清理：即便调用方异常返回未执行 stop()，也尽量移除 adb forward，避免端口泄漏。
        let adb_path = self.adb.adb_path.clone();
        let device_id = self.device_id.clone();
        for port in [self.actual_video_port, self.actual_control_port] {
            let local = format!("tcp:{}", port);
            let mut command = std::process::Command::new(&adb_path);
            command
                .args(["-s", &device_id, "forward", "--remove", &local])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            command.creation_flags(CREATE_NO_WINDOW);
            let _ = command.status();
        }
    }
}


