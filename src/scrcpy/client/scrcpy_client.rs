use std::path::PathBuf;

use crate::gh_common::{Result, ScrcpyError};
use crate::scrcpy::client::scrcpy_conn::ScrcpyConnect;
use crate::scrcpy::client::scrcpy_control::ControlChannel;
use crate::scrcpy::client::scrcpy_video_stream::FramedVideoStreamReader;
use crate::scrcpy::config::ScrcpyClientConfig;
use crate::scrcpy::session::Session;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

/// Scrcpy 客户端主对象。
///
/// 职责边界：
/// - 对上层提供统一入口；
/// - 消费 `ScrcpyClientConfig` 并构建底层连接对象；
/// - 负责连接建立与停止，连接成功后产出 `Session`。
#[derive(Debug, Clone)]
pub struct ScrcpyClient {
    /// 客户端初始化配置。
    config: ScrcpyClientConfig,
}

impl ScrcpyClient {
    /// 使用统一配置构建 ScrcpyClient。
    pub fn new(config: ScrcpyClientConfig) -> Self {
        Self { config }
    }

    /// 读取当前客户端配置快照。
    pub fn config(&self) -> &ScrcpyClientConfig {
        &self.config
    }

    /// 根据当前配置创建底层连接对象。
    pub fn build_scrcpy_connect(&self) -> Result<ScrcpyConnect> {
        let cfg = &self.config;

        let mut conn = ScrcpyConnect::with_config(
            PathBuf::from(cfg.adb_path.clone()),
            cfg.device_id.clone(),
            PathBuf::from(cfg.server_path.clone()),
            cfg.max_size,
            cfg.bit_rate,
            cfg.max_fps,
            cfg.video_port,
            cfg.control_port,
            cfg.intra_refresh_period,
            cfg.video_encoder.clone(),
        )?;

        // 对齐历史生产链路：默认启用 framed 协议。
        conn.set_framed_stream_enabled(true);
        Ok(conn)
    }

    /// 连接前执行启动策略。
    ///
    /// 该阶段尚未构建 `Session`，适合执行 adb 级策略。
    async fn apply_startup_policies_before_session(&self, conn: &ScrcpyConnect) -> Result<()> {
        if self.config.stay_awake {
            conn.set_stay_awake(true).await?;
        }
        Ok(())
    }

    /// 连接后执行启动策略。
    ///
    /// 该阶段 `Session` 已就绪，可以走控制通道能力。
    async fn apply_startup_policies_after_session(&self, session: &mut Session) -> Result<()> {
        if self.config.turn_screen_off {
            session.set_display_power(false).await?;
        }
        Ok(())
    }

    /// 启动期视频流探活。
    ///
    /// 某些设备会在握手后短时间内直接断流，
    /// 这里用 `peek` 做非破坏性探测，提前判失败给上层回退逻辑处理。
    async fn probe_video_stream_alive(stream: &TcpStream) -> Result<()> {
        let mut buf = [0u8; 1];
        match timeout(Duration::from_millis(800), stream.peek(&mut buf)).await {
            Err(_) => Ok(()),
            Ok(Ok(0)) => Err(ScrcpyError::Network(
                "视频流在启动探测阶段已关闭".to_string(),
            )),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(ScrcpyError::Network(format!(
                "视频流启动探测失败: {}",
                e
            ))),
        }
    }

    /// 单次建链尝试。
    async fn start_once(&self, mut conn: ScrcpyConnect) -> Result<Session> {
        conn.deploy().await?;
        conn.start().await?;

        debug!("[客户端] 连接视频与控制通道");
        let mut video_socket = conn.connect_video().await?;
        let control_socket = conn.connect_control().await?;

        ScrcpyConnect::read_video_header(&mut video_socket, conn.is_framed_stream_enabled())
            .await?;
        Self::probe_video_stream_alive(&video_socket).await?;
        self.apply_startup_policies_before_session(&conn).await?;

        let control = ControlChannel::new(control_socket);
        let video_stream = FramedVideoStreamReader::new(video_socket);
        let mut session = Session::from_connected(conn, control, video_stream);

        self.apply_startup_policies_after_session(&mut session).await?;
        Ok(session)
    }

    /// 使用给定连接对象建立连接，成功后返回会话。
    ///
    /// 生产回退策略：
    /// - 当 `intra_refresh_period == 1` 且首轮失败时，自动回退到 `0` 重试一次。
    pub async fn start(&self, conn: ScrcpyConnect) -> Result<Session> {
        info!("[客户端] 开始建立连接");

        if conn.intra_refresh_period() == 1 {
            match self.start_once(conn).await {
                Ok(session) => {
                    info!("[客户端] 连接成功");
                    return Ok(session);
                }
                Err(first_err) => {
                    warn!(
                        "[客户端] intra=1 建链失败，回退到 intra=0 重试一次: {}",
                        first_err
                    );

                    let mut retry_conn = self.build_scrcpy_connect()?;
                    retry_conn.set_intra_refresh_period(0);
                    let session = self.start_once(retry_conn).await?;
                    info!("[客户端] 连接成功（intra=0 回退）");
                    return Ok(session);
                }
            }
        }

        let session = self.start_once(conn).await?;
        info!("[客户端] 连接成功");
        Ok(session)
    }

    /// 停止并销毁会话。
    pub async fn stop(&self, session: &mut Session) -> Result<()> {
        info!("[客户端] 开始停止会话");
        session.dispose().await
    }
}


