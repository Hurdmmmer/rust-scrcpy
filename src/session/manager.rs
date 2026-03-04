use std::path::PathBuf;

use tokio::net::TcpStream;
use tokio::time::Duration;
use tracing::{info, warn};

use crate::adb::AdbClient;
use crate::config::config::DEFAULT_USE_FRAMED_STREAM;
use crate::error::{Result, ScrcpyError};
use crate::scrcpy::ScrcpyServer;
use crate::session::device_cache::{DeviceCache, DeviceProfileSnapshot};
use crate::session::encoding_profile::EncodingProfile;

/// 会话默认使用 scrcpy framed 协议。

/// 设备方向模式。
///
/// 说明：
/// - `Auto`：恢复系统自动旋转；
/// - `Portrait`：锁定竖屏（user_rotation=0）；
/// - `Landscape`：锁定横屏（user_rotation=1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenOrientationMode {
    Auto,
    Portrait,
    Landscape,
}

/// 单次会话中用于上层链路的设备画像。
///
/// 该结构体会直接返回给上层（测试、Flutter API），
/// 用于触控映射、窗口尺寸策略与日志追踪。
#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub device_id: String,
    pub model: String,
    pub android_version: String,
    pub screen_width: u32,
    pub screen_height: u32,
}

/// 建链成功后返回给调用方的会话句柄集合。
///
/// - `server`：scrcpy server 生命周期控制；
/// - `video_stream`：视频 NAL 读取通道；
/// - `control_stream`：触控/按键控制通道。
pub struct SessionConnection {
    pub server: ScrcpyServer,
    pub video_stream: TcpStream,
    pub control_stream: TcpStream,
}

/// 会话管理器。
///
/// 设计目标：
/// 1. 将设备探测、server 启动、双通道连接封装为稳定流程；
/// 2. 提供 `connect_v2` 最小连接策略，降低部分机型（如三星）崩溃概率；
/// 3. 提供方向控制 API，供测试与 Flutter 直接调用；
/// 4. 维护可选设备缓存，加速下一次会话初始化。
pub struct SessionManager {
    adb: AdbClient,
    device_id: String,
    server_path: PathBuf,
    video_port: u16,
    control_port: u16,
    profile: EncodingProfile,
    cache_path: Option<PathBuf>,
    cache: DeviceCache,
}

impl SessionManager {
    /// 创建会话管理器。
    ///
    /// `cache_path` 为 `None` 时不启用持久化缓存。
    pub fn new(
        adb: AdbClient,
        device_id: String,
        server_path: PathBuf,
        video_port: u16,
        control_port: u16,
        profile: EncodingProfile,
        cache_path: Option<PathBuf>,
    ) -> Result<Self> {
        let cache = if let Some(path) = &cache_path {
            DeviceCache::load(path)?
        } else {
            DeviceCache::default()
        };

        Ok(Self {
            adb,
            device_id,
            server_path,
            video_port,
            control_port,
            profile,
            cache_path,
            cache,
        })
    }

    /// 经典连接入口（历史兼容），当前未被主链路调用，先注释保留。
    // pub async fn connect(&mut self) -> Result<SessionConnection> {
    //     self.connect_v2().await
    // }

    /// v2 最小连接流程（生产推荐）。
    ///
    /// 关键策略：
    /// - 仅启用必要 server 参数；
    /// - 先建链再读取设备画像，减少建链前命令扰动；
    /// - 方向、熄屏等设备控制由独立 API 执行，避免和 server 启动耦合。
    pub async fn connect_v2(&mut self) -> Result<SessionConnection> {
        info!(
            "[Session] connect_v2 start: device={} intra_refresh_period={} turn_screen_off={} stay_awake={}",
            self.device_id,
            self.profile.intra_refresh_period,
            self.profile.turn_screen_off,
            self.profile.stay_awake
        );
        // 兼容策略（关键）：
        // - 默认按配置值启动（当前通常为 1）；
        // - 若启动阶段检测到流提前结束，且当前值为 1，则自动回退到 0 重试一次。
        if self.profile.intra_refresh_period == 1 {
            match self.connect_v2_once(1).await {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    warn!(
                        "[Session] connect_v2 with intra=1 failed, fallback to intra=0: {}",
                        e
                    );
                    return self.connect_v2_once(0).await;
                }
            }
        }

        self.connect_v2_once(self.profile.intra_refresh_period).await
    }

    /// 单次建链尝试（指定本次实际使用的 `intra_refresh_period`）。
    ///
    /// 注意：
    /// - 该函数只负责“一次尝试”，不包含 fallback；
    /// - fallback 由 `connect_v2()` 统一调度，避免重试逻辑分散。
    async fn connect_v2_once(&mut self, intra_refresh_period: u32) -> Result<SessionConnection> {
        let mut server = ScrcpyServer::with_config(
            self.adb.clone(),
            self.device_id.clone(),
            self.server_path.clone(),
            self.profile.max_size,
            self.profile.bit_rate,
            self.profile.max_fps,
            self.video_port,
            self.control_port,
            intra_refresh_period,
            self.profile.video_encoder.clone(),
        )?;

        // 协议模式开关（固定常量）：
        // - 默认启用 framed（生产推荐，包边界稳定）；
        // - 如需回退 raw，请改 DEFAULT_USE_FRAMED_STREAM 常量。
        server.set_framed_stream_enabled(DEFAULT_USE_FRAMED_STREAM);

        server.deploy().await?;
        server.start().await?;

        let mut video_stream = server.connect_video().await?;
        let control_stream = server.connect_control().await?;

        // 读取并消费 scrcpy 协议头（dummy byte）。
        // 如果这里不读取，后续 NAL 解析会从错误偏移开始。
        ScrcpyServer::read_video_header(&mut video_stream, server.is_framed_stream_enabled())
            .await?;

        // 启动健康检查（关键逻辑）：
        // - 某些机型在 intra=1 时会在“连上后极短时间内”直接断流；
        // - 这里用 peek() 非破坏性读取探测流是否提前 EOF；
        // - 若探测到 EOF，则让本次尝试失败，交由上层 fallback 到 intra=0。
        Self::probe_video_stream_alive(&video_stream).await?;

        // 建链后再进行设备画像采集，降低连接抖动。
        let device_profile = self.load_or_probe_device_profile().await?;
        info!(
            "[Session] device profile: model={} android={} size={}x{}",
            device_profile.model,
            device_profile.android_version,
            device_profile.screen_width,
            device_profile.screen_height
        );
        if self.profile.force_landscape {
            self.set_screen_orientation_mode(ScreenOrientationMode::Landscape)
                .await?;
        }

        if self.profile.stay_awake {
            self.adb
                .shell(&self.device_id, "svc power stayon true")
                .await?;
        }

        // 注意：
        // - 这里不直接执行 adb keyevent 熄屏；
        // - 熄屏由 runtime 层通过 scrcpy 控制协议 set_display_power(false) 触发，
        //   以确保“投屏继续，设备屏幕熄灭”的语义。

        info!(
            "[Session] connect_v2 success: device={} intra_refresh_period={}",
            self.device_id, intra_refresh_period
        );

        Ok(SessionConnection {
            server,
            video_stream,
            control_stream,
        })
    }

    /// 非破坏性探测视频流是否在启动阶段提前结束。
    ///
    /// 返回语义：
    /// - `Ok(())`：流仍可读（或暂时无数据但未 EOF）；
    /// - `Err(...)`：已 EOF 或探测异常，调用方应判定为本次建链失败。
    async fn probe_video_stream_alive(stream: &TcpStream) -> Result<()> {
        let mut buf = [0u8; 1];
        match tokio::time::timeout(Duration::from_millis(800), stream.peek(&mut buf)).await {
            // 800ms 内无新字节，不判失败（避免误伤首帧较慢机型）。
            Err(_) => Ok(()),
            Ok(Ok(0)) => Err(ScrcpyError::Network(
                "video stream closed during startup probe".to_string(),
            )),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(ScrcpyError::Network(format!(
                "video stream startup probe failed: {}",
                e
            ))),
        }
    }


    /// 设置设备方向模式。
    ///
    /// 注意：该接口只负责“设备侧方向锁定”，
    /// 视频解码/渲染链路的分辨率切换与重建由上层自行处理。
    pub async fn set_screen_orientation_mode(&self, mode: ScreenOrientationMode) -> Result<()> {
        match mode {
            ScreenOrientationMode::Auto => {
                self.adb
                    .shell(&self.device_id, "settings put system accelerometer_rotation 1")
                    .await?;
            }
            ScreenOrientationMode::Portrait => {
                self.adb
                    .shell(&self.device_id, "settings put system accelerometer_rotation 0")
                    .await?;
                self.adb
                    .shell(&self.device_id, "settings put system user_rotation 0")
                    .await?;
            }
            ScreenOrientationMode::Landscape => {
                self.adb
                    .shell(&self.device_id, "settings put system accelerometer_rotation 0")
                    .await?;
                self.adb
                    .shell(&self.device_id, "settings put system user_rotation 1")
                    .await?;
            }
        }

        Ok(())
    }

    async fn load_or_probe_device_profile(&mut self) -> Result<DeviceProfile> {
        if let Some(cached) = self.cache.get(&self.device_id) {
            return Ok(DeviceProfile {
                device_id: cached.device_id.clone(),
                model: cached.model.clone(),
                android_version: cached.android_version.clone(),
                screen_width: cached.screen_width,
                screen_height: cached.screen_height,
            });
        }

        let model = self
            .adb
            .shell(&self.device_id, "getprop ro.product.model")
            .await?
            .trim()
            .to_string();

        let android_version = self
            .adb
            .shell(&self.device_id, "getprop ro.build.version.release")
            .await?
            .trim()
            .to_string();

        let wm_size_output = self.adb.shell(&self.device_id, "wm size").await?;
        let (screen_width, screen_height) = parse_wm_size(&wm_size_output)?;

        let profile = DeviceProfile {
            device_id: self.device_id.clone(),
            model,
            android_version,
            screen_width,
            screen_height,
        };

        self.cache.upsert(DeviceProfileSnapshot {
            device_id: profile.device_id.clone(),
            model: profile.model.clone(),
            android_version: profile.android_version.clone(),
            screen_width: profile.screen_width,
            screen_height: profile.screen_height,
        });

        if let Some(path) = &self.cache_path {
            if let Err(e) = self.cache.save(path) {
                warn!("save device cache failed: {}", e);
            }
        }

        Ok(profile)
    }

}

/// 解析 `adb shell wm size` 输出中的物理分辨率。
///
/// 典型输出：
/// - `Physical size: 1440x3200`
/// - `Override size: 1080x2400`（如存在 override，优先 Physical）
fn parse_wm_size(output: &str) -> Result<(u32, u32)> {
    let mut candidate = None;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // 优先读取 Physical size。
        let text = if let Some(v) = line.strip_prefix("Physical size:") {
            v.trim()
        } else if candidate.is_none() {
            // 兜底：如果没有 Physical，就尝试第一个可解析尺寸。
            line
        } else {
            continue;
        };

        if let Some((w, h)) = parse_size_pair(text) {
            if line.starts_with("Physical size:") {
                return Ok((w, h));
            }
            candidate = Some((w, h));
        }
    }

    candidate.ok_or_else(|| ScrcpyError::Parse(format!("无法解析 wm size 输出: {}", output)))
}

fn parse_size_pair(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split('x');
    let w = parts.next()?.trim().parse::<u32>().ok()?;
    let h = parts.next()?.trim().parse::<u32>().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}
