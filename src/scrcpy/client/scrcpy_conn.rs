use crate::gh_common::find_available_port;
use crate::gh_common::{Result, ScrcpyError};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tracing::{info, warn};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";
const SOCKET_NAME: &str = "scrcpy";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// scrcpy 连接对象。
///
/// 职责：
/// - 负责 server 部署、启动和端口转发；
/// - 负责视频/控制通道连接与协议头读取；
/// - 负责会话结束时的进程与 forward 清理。
pub struct ScrcpyConnect {
    /// adb 可执行文件路径。
    adb_path: PathBuf,
    /// 设备 ID（adb -s 参数）。
    device_id: String,
    /// 本地 scrcpy-server.jar 路径。
    server_path: PathBuf,
    /// 期望视频端口。
    video_port: u16,
    /// 实际视频端口（自动避让冲突后）。
    actual_video_port: u16,
    /// 期望控制端口。
    control_port: u16,
    /// 实际控制端口（自动避让冲突后）。
    actual_control_port: u16,
    /// scrcpy max_size 参数。
    max_size: u32,
    /// scrcpy video_bit_rate 参数。
    bit_rate: u32,
    /// scrcpy max_fps 参数。
    max_fps: u32,
    /// 强制关键帧间隔（秒）。
    intra_refresh_period: u32,
    /// 可选视频编码器。
    video_encoder: Option<String>,
    /// 是否启用 framed 协议。
    use_framed_stream: bool,
    /// server 子进程句柄。
    server_process: Option<Child>,
}

impl ScrcpyConnect {
    /// 创建带配置的连接对象，并自动分配可用端口。
    pub fn with_config(
        adb_path: PathBuf,
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
        let actual_video_port = find_available_port(video_port, 100)?;
        let actual_control_port = find_available_port(
            if control_port <= actual_video_port {
                actual_video_port + 1
            } else {
                control_port
            },
            100,
        )?;

        Ok(Self {
            adb_path,
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

    /// 读取当前关键帧间隔配置。
    pub fn intra_refresh_period(&self) -> u32 {
        self.intra_refresh_period
    }

    /// 更新关键帧间隔配置。
    pub fn set_intra_refresh_period(&mut self, value: u32) {
        self.intra_refresh_period = value;
    }

    /// 设置视频流协议模式。
    ///
    /// - `true`：framed（raw_stream=false）；
    /// - `false`：raw（raw_stream=true）。
    pub fn set_framed_stream_enabled(&mut self, enabled: bool) {
        self.use_framed_stream = enabled;
    }

    /// 查询当前是否启用 framed 协议。
    pub fn is_framed_stream_enabled(&self) -> bool {
        self.use_framed_stream
    }

    /// 执行 adb 命令并返回 stdout。
    async fn adb_execute(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new(&self.adb_path);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let output = command
            .output()
            .await
            .map_err(|e| ScrcpyError::Adb(format!("执行 ADB 失败: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ScrcpyError::Adb(format!("ADB 命令失败: {}", stderr)));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// 推送文件到设备。
    async fn adb_push(&self, local: &str, remote: &str) -> Result<()> {
        self.adb_execute(&["-s", &self.device_id, "push", local, remote])
            .await?;
        Ok(())
    }

    /// 添加端口转发。
    async fn adb_forward(&self, local_port: u16, remote: &str) -> Result<()> {
        let local = format!("tcp:{}", local_port);
        self.adb_execute(&["-s", &self.device_id, "forward", &local, remote])
            .await?;
        Ok(())
    }

    /// 删除端口转发。
    async fn adb_forward_remove(&self, local_port: u16) -> Result<()> {
        let local = format!("tcp:{}", local_port);
        self.adb_execute(&["-s", &self.device_id, "forward", "--remove", &local])
            .await?;
        Ok(())
    }

    /// 启用/关闭设备防休眠。
    pub async fn set_stay_awake(&self, enabled: bool) -> Result<()> {
        let value = if enabled { "true" } else { "false" };
        self.adb_execute(&[
            "-s",
            &self.device_id,
            "shell",
            &format!("svc power stayon {}", value),
        ])
        .await?;
        Ok(())
    }

    /// 执行方向锁定。
    pub async fn set_orientation_auto(&self) -> Result<()> {
        self.adb_execute(&[
            "-s",
            &self.device_id,
            "shell",
            "settings put system accelerometer_rotation 1",
        ])
        .await?;
        Ok(())
    }

    /// 锁定竖屏。
    pub async fn set_orientation_portrait(&self) -> Result<()> {
        self.adb_execute(&[
            "-s",
            &self.device_id,
            "shell",
            "settings put system accelerometer_rotation 0",
        ])
        .await?;
        self.adb_execute(&[
            "-s",
            &self.device_id,
            "shell",
            "settings put system user_rotation 0",
        ])
        .await?;
        Ok(())
    }

    /// 锁定横屏。
    pub async fn set_orientation_landscape(&self) -> Result<()> {
        self.adb_execute(&[
            "-s",
            &self.device_id,
            "shell",
            "settings put system accelerometer_rotation 0",
        ])
        .await?;
        self.adb_execute(&[
            "-s",
            &self.device_id,
            "shell",
            "settings put system user_rotation 1",
        ])
        .await?;
        Ok(())
    }

    /// 部署 server 到设备。
    pub async fn deploy(&self) -> Result<()> {
        info!("[连接] 开始部署 scrcpy-server");

        if !self.server_path.exists() {
            return Err(ScrcpyError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("未找到 server 文件: {:?}", self.server_path),
            )));
        }

        let local_path = self
            .server_path
            .to_str()
            .ok_or_else(|| ScrcpyError::Parse("server 路径无效".to_string()))?;

        info!("[连接] 推送 server 到设备: {}", local_path);
        self.adb_push(local_path, DEVICE_SERVER_PATH).await?;

        info!("[连接] 部署完成");
        Ok(())
    }

    /// 启动 scrcpy server。
    pub async fn start(&mut self) -> Result<()> {
        info!(
            "[连接] 启动 server: video_port={} (req={}), control_port={} (req={})",
            self.actual_video_port, self.video_port, self.actual_control_port, self.control_port
        );

        self.adb_forward(
            self.actual_video_port,
            &format!("localabstract:{}", SOCKET_NAME),
        )
        .await?;

        self.adb_forward(
            self.actual_control_port,
            &format!("localabstract:{}", SOCKET_NAME),
        )
        .await?;

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

        info!("[连接] 执行 server 命令: shell {}", server_args);

        let mut command = Command::new(&self.adb_path);
        command
            .args(["-s", &self.device_id, "shell", &server_args])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = command
            .spawn()
            .map_err(|e| ScrcpyError::Adb(format!("启动 server 失败: {}", e)))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    warn!("[连接] server stderr: {}", line.trim());
                    line.clear();
                }
            });
        }

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
                        info!("[连接] server 输出: {}", text);
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    line.clear();
                }
            });
        }

        let wait_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
        let mut readiness_received = false;
        loop {
            if tokio::time::Instant::now() >= wait_deadline {
                warn!("[连接] 等待 server 就绪超时，进入兜底流程");
                break;
            }

            if ready_rx.try_recv().is_ok() {
                info!("[连接] 收到 server 就绪信号");
                readiness_received = true;
                break;
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(ScrcpyError::Adb(format!(
                        "server 在就绪前异常退出: {}",
                        status
                    )));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("[连接] 查询 server 状态失败: {}", e);
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        self.server_process = Some(child);

        let settle_ms = if readiness_received { 120 } else { 500 };
        info!("[连接] server 启动缓冲等待: {}ms", settle_ms);
        tokio::time::sleep(tokio::time::Duration::from_millis(settle_ms)).await;

        info!("[连接] server 启动完成，video_port={}", self.actual_video_port);
        Ok(())
    }

    /// 建立视频通道连接。
    pub async fn connect_video(&self) -> Result<TcpStream> {
        let addr = format!("127.0.0.1:{}", self.actual_video_port);
        info!("[连接] 连接视频通道: {}", addr);

        let mut stream = None;
        for attempt in 1..=5 {
            info!("[连接] 视频通道连接尝试 {}/5", attempt);
            match TcpStream::connect(&addr).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) if attempt < 5 => {
                    warn!("[连接] 视频通道连接失败，准备重试: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    return Err(ScrcpyError::Network(format!(
                        "视频通道连接失败（重试5次后）: {}",
                        e
                    )));
                }
            }
        }

        info!("[连接] 视频通道连接成功");
        Ok(stream.expect("video stream must exist after retry loop"))
    }

    /// 建立控制通道连接。
    pub async fn connect_control(&self) -> Result<TcpStream> {
        let addr = format!("127.0.0.1:{}", self.actual_control_port);
        info!("[连接] 连接控制通道: {}", addr);

        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| ScrcpyError::Network(format!("控制通道连接失败: {}", e)))?;

        info!("[连接] 控制通道连接成功");
        Ok(stream)
    }

    /// 读取并消费 scrcpy 视频头。
    ///
    /// - raw 模式：读取 dummy byte；
    /// - framed 模式：读取 dummy byte + codec meta(12字节)。
    pub async fn read_video_header(stream: &mut TcpStream, expect_codec_meta: bool) -> Result<()> {
        info!("[连接] 读取视频协议头");

        let mut dummy_byte = [0u8; 1];
        stream
            .read_exact(&mut dummy_byte)
            .await
            .map_err(|e| ScrcpyError::Network(format!("读取 dummy byte 失败: {}", e)))?;

        if expect_codec_meta {
            let mut meta = [0u8; 12];
            stream
                .read_exact(&mut meta)
                .await
                .map_err(|e| ScrcpyError::Network(format!("读取 codec meta 失败: {}", e)))?;

            let codec_id = u32::from_be_bytes([meta[0], meta[1], meta[2], meta[3]]);
            let width = u32::from_be_bytes([meta[4], meta[5], meta[6], meta[7]]);
            let height = u32::from_be_bytes([meta[8], meta[9], meta[10], meta[11]]);

            info!(
                "[连接] 视频协议头读取成功（framed）: codec=0x{:08x}, {}x{}",
                codec_id, width, height
            );
            return Ok(());
        }

        info!("[连接] 视频协议头读取成功（raw）");
        Ok(())
    }

    /// 停止 server 并清理端口转发。
    pub async fn stop(&mut self) -> Result<()> {
        info!("[连接] 停止 server");

        if let Some(mut child) = self.server_process.take() {
            let _ = child.kill().await;
        }

        let _ = self.adb_forward_remove(self.actual_video_port).await;
        let _ = self.adb_forward_remove(self.actual_control_port).await;

        info!("[连接] server 已停止");
        Ok(())
    }
}

impl Drop for ScrcpyConnect {
    fn drop(&mut self) {
        if let Some(mut child) = self.server_process.take() {
            let _ = child.start_kill();
        }

        let adb_path = self.adb_path.clone();
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

