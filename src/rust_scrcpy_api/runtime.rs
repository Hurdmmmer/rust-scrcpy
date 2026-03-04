//! API 运行时层：承载会话运行时实现与轮询行为。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use crate::adb::AdbClient;
use crate::decoder::{
    DecodedFrame, DecoderPipeline, DecoderPreference, PipelineConfig, PipelineEvent,
};
use crate::error::{Result, ScrcpyError};
use crate::scrcpy::control::{
    AndroidKeyEventAction, ControlChannel, KeyEvent, ScrollEvent, TouchEvent,
};
use crate::scrcpy::FramedVideoStreamReader;
use crate::session::manager::{ScreenOrientationMode, SessionManager};
use crate::session::encoding_profile::EncodingProfile;
use super::{
    DecoderMode, ErrorCode, OrientationChangeSource, OrientationMode, RenderPipelineMode,
    SessionConfig, SessionEvent, SessionStats, SystemKey, TextureFrame,
};
use crate::flutter_callback_register;
/// 会话运行时抽象层（生产级骨架）。
///
/// 目标：
/// 1. 对外 API 保持稳定；
/// 2. 底层实现可按阶段替换（占位 -> 真实 scrcpy 会话 -> GPU 直出）；
/// 3. 自动旋转/主动旋转/重同步行为都通过事件统一输出。
/// 会话运行时 trait。
/// 说明：FRB 生成代码位于同一 crate 的其它模块，需要可见性为 `pub(crate)`。
pub(crate) trait SessionRuntime {
    /// 启动运行时：拉起 worker、建立 scrcpy 链路并开始解码轮询。
    fn start(&mut self) -> Result<()>;
    /// 停止运行时：停止 worker 并释放会话级临时资源。
    fn stop(&mut self) -> Result<()>;
    /// 拉取可渲染帧（轮询语义）。
    fn poll_texture_frames(&mut self) -> Result<Vec<TextureFrame>>;
    /// 拉取会话事件（轮询语义）。
    fn poll_session_events(&mut self) -> Result<Vec<SessionEvent>>;
    /// 返回统计快照。
    fn stats(&self) -> SessionStats;
    /// 是否处于运行态。
    fn is_running(&self) -> bool;
    /// 设置方向意图。
    fn set_orientation_mode(&mut self, mode: OrientationMode) -> Result<()>;
    /// 发送触摸事件。
    fn send_touch(&mut self, event: TouchEvent) -> Result<()>;
    /// 发送按键事件。
    fn send_key(&mut self, event: KeyEvent) -> Result<()>;
    /// 发送滚动事件。
    fn send_scroll(&mut self, event: ScrollEvent) -> Result<()>;
    /// 发送文本输入。
    fn send_text(&mut self, text: String) -> Result<()>;
    /// 发送系统按键语义事件。
    fn send_system_key(&mut self, key: SystemKey) -> Result<()>;
    /// 设置剪贴板。
    fn set_clipboard(&mut self, text: String, paste: bool) -> Result<()>;
    /// 请求关键帧（IDR）。
    fn request_idr(&mut self) -> Result<()>;
}

#[derive(Debug)]
enum RuntimeCommand {
    Stop,
    Touch(TouchEvent),
    Key(KeyEvent),
    Scroll(ScrollEvent),
    Text(String),
    Clipboard { text: String, paste: bool },
    SystemKey(SystemKey),
    SetOrientation(OrientationMode),
    RequestIdr,
}

/// 硬解快速恢复状态：
/// - 首次解码失败后，先在同会话内请求 IDR 并等待短窗口；
/// - 若窗口内收到“已重同步”信号，则继续当前会话；
/// - 若超时仍未恢复，再升级为整会话重连。
#[derive(Debug, Clone, Copy)]
struct HwRecoveryState {
    fail_signal: u64,
    start_at: Instant,
    deadline_at: Instant,
}

/// 真实运行时：
/// - 建立 scrcpy 会话；
/// - 读取分帧视频包并进入解码流水线；
/// - 输出真实共享纹理句柄帧；
/// - 通过控制通道执行触控/按键/旋转/IDR。
/// 真实会话运行时。
/// 说明：FRB 自动生成代码会引用该类型，因此暴露为 `pub(crate)` 供 crate 内访问。
pub(crate) struct RealSessionRuntime {
    /// API 层会话 ID（sess-*），用于回调链路标识事件归属。
    session_id: String,
    config: SessionConfig,
    decoder_mode: DecoderMode,
    render_pipeline_mode: RenderPipelineMode,
    running: Arc<AtomicBool>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    frames: Arc<Mutex<VecDeque<TextureFrame>>>,
    flow_monitor: Arc<FlowMonitor>,
    stats: Arc<Mutex<SessionStats>>,
    generation: Arc<AtomicU64>,
    cmd_tx: Option<Sender<RuntimeCommand>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
/// 链路流量监控器。
/// 说明：FRB 自动生成代码在 RustOpaque 场景下会引用该类型，因此需 `pub(crate)`。
pub(crate) struct FlowMonitor {
    enqueued_total: AtomicU64,
    replaced_same_generation: AtomicU64,
    queue_trim_dropped: AtomicU64,
    invalid_frame_dropped: AtomicU64,
    stale_generation_dropped: AtomicU64,
    polled_total: AtomicU64,
    polled_latest_returned: AtomicU64,
    polled_stale_dropped: AtomicU64,
    last_consumer_warn_ms: AtomicU64,
}

impl RealSessionRuntime {
    /// 将 BGRA8 像素缓冲转换为 RGBA8（同尺寸）。
    fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(src.len());
        for px in src.chunks_exact(4) {
            out.push(px[2]); // R
            out.push(px[1]); // G
            out.push(px[0]); // B
            out.push(px[3]); // A
        }
        out
    }

    /// 创建运行时对象（仅初始化内存状态，不启动线程）。
    pub(crate) fn new(session_id: String, config: SessionConfig) -> Self {
        Self::new_with_options(
            session_id,
            config,
            DecoderMode::ForceSoftware,
            RenderPipelineMode::Original,
        )
    }

    /// 创建运行时对象（V2，支持显式解码/渲染模式配置）。
    pub(crate) fn new_with_options(
        session_id: String,
        config: SessionConfig,
        decoder_mode: DecoderMode,
        render_pipeline_mode: RenderPipelineMode,
    ) -> Self {
        Self {
            session_id,
            config,
            decoder_mode,
            render_pipeline_mode,
            running: Arc::new(AtomicBool::new(false)),
            events: Arc::new(Mutex::new(VecDeque::new())),
            frames: Arc::new(Mutex::new(VecDeque::new())),
            flow_monitor: Arc::new(FlowMonitor::default()),
            stats: Arc::new(Mutex::new(SessionStats {
                fps: 0.0,
                decode_latency_ms: 0,
                upload_latency_ms: 0,
                total_frames: 0,
                dropped_frames: 0,
            })),
            generation: Arc::new(AtomicU64::new(1)),
            cmd_tx: None,
            worker: None,
        }
    }

    fn map_decoder_mode(mode: DecoderMode) -> DecoderPreference {
        match mode {
            DecoderMode::PreferHardware => DecoderPreference::PreferHardware,
            DecoderMode::ForceHardware => DecoderPreference::ForceHardware,
            DecoderMode::ForceSoftware => DecoderPreference::ForceSoftware,
        }
    }

    /// 推送会话事件到环形队列，超过上限时丢弃最旧事件。
    fn push_event(session_id: &str, events: &Arc<Mutex<VecDeque<SessionEvent>>>, event: SessionEvent) {
        // 统一序列化并推送到 C 回调链路（Rust -> Runner -> Dart）。
        if let Ok(payload) = serde_json::to_vec(&event) {
            flutter_callback_register::notify_session_event(session_id, &payload);
        }

        if let Ok(mut q) = events.lock() {
            q.push_back(event);
            while q.len() > 256 {
                let _ = q.pop_front();
            }
        }
    }

    /// 推送纹理帧到队列，采用“同代覆盖 + 小队列截断”策略控制延迟。
    fn push_frame(
        frames: &Arc<Mutex<VecDeque<TextureFrame>>>,
        flow_monitor: &Arc<FlowMonitor>,
        frame: TextureFrame,
    ) {
        flow_monitor.enqueued_total.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut q) = frames.lock() {
            // 生产低延迟策略：
            // 1) 相同代际只保留最新帧（覆盖队尾），避免 Flutter 端处理历史帧；
            // 2) 跨代际最多保留最近 2 帧（旧代际过渡帧 + 新代际最新帧）。
            if let Some(back) = q.back_mut() {
                if back.generation == frame.generation {
                    *back = frame;
                    flow_monitor
                        .replaced_same_generation
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            q.push_back(frame);
            while q.len() > 2 {
                let _ = q.pop_front();
                flow_monitor.queue_trim_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// 获取当前 Unix 毫秒时间戳。
    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// 线程安全地更新统计快照。
    fn update_stats(stats: &Arc<Mutex<SessionStats>>, f: impl FnOnce(&mut SessionStats)) {
        if let Ok(mut s) = stats.lock() {
            f(&mut s);
        }
    }

    /// 将 API 层方向模式映射到会话管理器方向模式。
    fn map_orientation(mode: OrientationMode) -> ScreenOrientationMode {
        match mode {
            OrientationMode::Auto => ScreenOrientationMode::Auto,
            OrientationMode::Portrait => ScreenOrientationMode::Portrait,
            OrientationMode::Landscape => ScreenOrientationMode::Landscape,
        }
    }

    /// 向运行时 worker 下发命令。
    fn send_runtime_cmd(&mut self, cmd: RuntimeCommand) -> Result<()> {
        if let Some(tx) = &self.cmd_tx {
            tx.send(cmd)
                .map_err(|e| ScrcpyError::Other(format!("runtime command send failed: {}", e)))
        } else {
            Err(ScrcpyError::Other("session runtime not started".to_string()))
        }
    }

    /// 将系统按键语义翻译为底层控制通道按键序列并发送。
    async fn send_system_key_async(control: &mut ControlChannel, key: SystemKey) -> Result<()> {
        match key {
            SystemKey::Home => control.send_home_key().await?,
            SystemKey::Back => control.send_back_key().await?,
            SystemKey::Recent => {
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Down,
                        keycode: 187,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Up,
                        keycode: 187,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
            }
            SystemKey::PowerMenu => {
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Down,
                        keycode: 26,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Up,
                        keycode: 26,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
            }
            SystemKey::VolumeUp => {
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Down,
                        keycode: 24,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Up,
                        keycode: 24,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
            }
            SystemKey::VolumeDown => {
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Down,
                        keycode: 25,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
                control
                    .send_key_event(&KeyEvent {
                        action: AndroidKeyEventAction::Up,
                        keycode: 25,
                        repeat: 0,
                        metastate: 0,
                    })
                    .await?;
            }
            SystemKey::RotateScreen => {}
        }
        Ok(())
    }
}

impl SessionRuntime for RealSessionRuntime {
    /// 启动会话 worker。
    ///
    /// 流程：
    /// 1) 建立 ADB/scrcpy 视频与控制链路；
    /// 2) 启动解码管线；
    /// 3) 在循环中消费命令、视频包与解码输出；
    /// 4) 产出帧队列与事件队列供 FFI 轮询。
    fn start(&mut self) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Ok(());
        }
        // 生产兜底：
        // 上一轮运行可能已经将 running 置为 false，但 worker 线程还在做收尾。
        // 复用同一 sessionId 直接 start 时，先回收旧 worker，避免并发双 worker。
        if let Some(prev_worker) = self.worker.take() {
            let _ = prev_worker.join();
        }
        self.cmd_tx = None;

        let (cmd_tx, cmd_rx): (Sender<RuntimeCommand>, Receiver<RuntimeCommand>) = mpsc::channel();
        self.cmd_tx = Some(cmd_tx);
        self.running.store(true, Ordering::Relaxed);
        Self::push_event(&self.session_id, &self.events, SessionEvent::Starting);

        let session_id = self.session_id.clone();
        let cfg = self.config.clone();
        let decoder_mode = self.decoder_mode;
        let render_pipeline_mode = self.render_pipeline_mode;
        let running = Arc::clone(&self.running);
        let events = Arc::clone(&self.events);
        let frames = Arc::clone(&self.frames);
        let flow_monitor = Arc::clone(&self.flow_monitor);
        let stats = Arc::clone(&self.stats);
        let generation = Arc::clone(&self.generation);

        self.worker = Some(std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(v) => v,
                Err(e) => {
                    Self::push_event(
                        &session_id,
                        &events,
                        SessionEvent::Error {
                            code: ErrorCode::Internal,
                            message: format!("创建 tokio runtime 失败: {}", e),
                        },
                    );
                    running.store(false, Ordering::Relaxed);
                    return;
                }
            };

            rt.block_on(async move {
                let adb = AdbClient::new(PathBuf::from(cfg.adb_path.clone()));
                let mut manager = match SessionManager::new(
                    adb,
                    cfg.device_id.clone(),
                    PathBuf::from(cfg.server_path.clone()),
                    cfg.video_port,
                    cfg.control_port,
                    EncodingProfile {
                        max_size: cfg.max_size,
                        bit_rate: cfg.bit_rate,
                        max_fps: cfg.max_fps,
                        intra_refresh_period: cfg.intra_refresh_period,
                        video_encoder: cfg.video_encoder.clone(),
                        turn_screen_off: cfg.turn_screen_off,
                        stay_awake: cfg.stay_awake,
                        force_landscape: false,
                        scrcpy_log_level: cfg.scrcpy_verbosity.clone(),
                    },
                    None,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        Self::push_event(
                            &session_id,
                            &events,
                            SessionEvent::Error {
                                code: ErrorCode::Internal,
                                message: format!("会话管理器创建失败: {}", e),
                            },
                        );
                        running.store(false, Ordering::Relaxed);
                        return;
                    }
                };

                let session = match manager.connect_v2().await {
                    Ok(v) => v,
                    Err(e) => {
                        Self::push_event(
                            &session_id,
                            &events,
                            SessionEvent::Error {
                                code: ErrorCode::DeviceDisconnected,
                                message: format!("connect_v2 失败: {}", e),
                            },
                        );
                        running.store(false, Ordering::Relaxed);
                        return;
                    }
                };

                let mut server = session.server;
                let mut reader = FramedVideoStreamReader::new(session.video_stream);
                let mut control = ControlChannel::new(session.control_stream);
                // 熄屏策略（正确链路）：
                // - 使用 scrcpy 控制协议 set_display_power(false)；
                // - 目标是“投屏继续显示，设备物理屏幕熄灭”；
                // - 失败时只告警，不中断会话。
                if cfg.turn_screen_off {
                    if let Err(e) = control.set_display_power(false).await {
                        warn!("set_display_power(false) failed: {}", e);
                    }
                }
                // 低延迟模式：将 handoff 队列从 64 降到 2，减少排队延迟。
                let (decoded_tx, decoded_rx) = mpsc::sync_channel::<DecodedFrame>(2);
                let (event_tx, event_rx) = mpsc::sync_channel::<PipelineEvent>(64);
                let decoded_tx_full_count = Arc::new(AtomicU64::new(0));
                let decoded_tx_full_count_for_closure = Arc::clone(&decoded_tx_full_count);
                let decoder_output_mode = match render_pipeline_mode {
                    RenderPipelineMode::Original => crate::decoder::DecoderOutputMode::GpuShared,
                    RenderPipelineMode::CpuPixelBufferV2 => {
                        crate::decoder::DecoderOutputMode::CpuBgra
                    }
                };
                let pipeline_cfg = PipelineConfig {
                    // Original: 共享句柄输出。
                    // CpuPixelBufferV2: CPU BGRA 输出。
                    frame_queue_capacity: 2,
                    nal_queue_capacity: 128,
                    drop_nal_on_full: false,
                    decoder_preference: Self::map_decoder_mode(decoder_mode),
                    decoder_output_mode,
                    ..PipelineConfig::default()
                };
                let pipeline = match DecoderPipeline::start_with_events(
                    pipeline_cfg,
                    move |frame| {
                        match decoded_tx.try_send(frame) {
                            Ok(()) => Ok(()),
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                let dropped = decoded_tx_full_count_for_closure
                                    .fetch_add(1, Ordering::Relaxed)
                                    + 1;
                                if dropped % 120 == 0 {
                                    warn!(
                                        "decoded_tx full: dropped_frames_on_handoff={}",
                                        dropped
                                    );
                                }
                                Ok(())
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(
                                ScrcpyError::Other(
                                    "decoded frame send disconnected".to_string(),
                                ),
                            ),
                        }
                    },
                    move |event| {
                        match event_tx.try_send(event) {
                            Ok(()) => Ok(()),
                            Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(()),
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(
                                ScrcpyError::Other(
                                    "pipeline event send disconnected".to_string(),
                                ),
                            ),
                        }
                    },
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = server.stop().await;
                        Self::push_event(
                            &session_id,
                            &events,
                            SessionEvent::Error {
                                code: ErrorCode::DecodeFailed,
                                message: format!("解码流水线初始化失败: {}", e),
                            },
                        );
                        running.store(false, Ordering::Relaxed);
                        return;
                    }
                };

                Self::push_event(&session_id, &events, SessionEvent::Running);
                let mut current_orientation = OrientationMode::Auto;
                let mut fps_frames = 0u64;
                let mut fps_tick = Instant::now();
                let mut last_size: (u32, u32) = (0, 0);
                let mut active_generation: u64 = generation.load(Ordering::Relaxed);
                let mut packet_total: u64 = 0;
                let mut handled_need_idr_signals: u64 = 0;
                let mut handled_resync_signals: u64 = 0;
                // 硬解重同步等待窗口（生产参数）：短窗口优先保障体感，超时再重连。
                const HW_RECOVERY_WAIT_MS: u64 = 220;
                let mut hw_recovery: Option<HwRecoveryState> = None;
                // let mut packet_delta_base: u64 = 0;
                // let mut frame_delta_base: u64 = 0;

                while running.load(Ordering::Relaxed) {
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(RuntimeCommand::Stop) => {
                                running.store(false, Ordering::Relaxed);
                                break;
                            }
                            Ok(RuntimeCommand::Touch(ev)) => {
                                if let Err(e) = control.send_touch_event(&ev).await {
                                    warn!("send_touch_event failed: {}", e);
                                }
                            }
                            Ok(RuntimeCommand::Key(ev)) => {
                                if let Err(e) = control.send_key_event(&ev).await {
                                    warn!("send_key_event failed: {}", e);
                                }
                            }
                            Ok(RuntimeCommand::Scroll(ev)) => {
                                if let Err(e) = control
                                    .send_scroll_event(
                                        ev.x, ev.y, ev.width, ev.height, ev.hscroll, ev.vscroll,
                                    )
                                    .await
                                {
                                    warn!("send_scroll_event failed: {}", e);
                                }
                            }
                            Ok(RuntimeCommand::Text(text)) => {
                                if let Err(e) = control.send_text(&text).await {
                                    warn!("send_text failed: {}", e);
                                }
                            }
                            Ok(RuntimeCommand::Clipboard { text, paste }) => {
                                if let Err(e) = control.set_clipboard(&text, paste).await {
                                    warn!("set_clipboard failed: {}", e);
                                }
                            }
                            Ok(RuntimeCommand::SystemKey(key)) => {
                                if matches!(key, SystemKey::RotateScreen) {
                                    let next = match current_orientation {
                                        OrientationMode::Landscape => OrientationMode::Portrait,
                                        _ => OrientationMode::Landscape,
                                    };
                                    let _ = manager
                                        .set_screen_orientation_mode(Self::map_orientation(next))
                                        .await;
                                    current_orientation = next;
                                    Self::push_event(
                                        &session_id,
                                        &events,
                                        SessionEvent::OrientationChanged {
                                            mode: next,
                                            source: OrientationChangeSource::ManualApi,
                                        },
                                    );
                                    let _ = control.send_reset_video().await;
                                } else {
                                    let _ = Self::send_system_key_async(&mut control, key).await;
                                    // 返回主页/返回上级/任务切换通常伴随大场景切换，主动请求一帧 IDR
                                    // 可显著缩短花屏恢复时间。
                                    if matches!(
                                        key,
                                        SystemKey::Home | SystemKey::Back | SystemKey::Recent
                                    ) {
                                        let _ = control.send_reset_video().await;
                                    }
                                }
                            }
                            Ok(RuntimeCommand::SetOrientation(mode)) => {
                                let _ = manager
                                    .set_screen_orientation_mode(Self::map_orientation(mode))
                                    .await;
                                current_orientation = mode;
                                Self::push_event(
                                    &session_id,
                                    &events,
                                    SessionEvent::OrientationChanged {
                                        mode,
                                        source: OrientationChangeSource::ManualApi,
                                    },
                                );
                                let _ = control.send_reset_video().await;
                            }
                            Ok(RuntimeCommand::RequestIdr) => {
                                if let Err(e) = control.send_reset_video().await {
                                    warn!("request_idr(send_reset_video) failed: {}", e);
                                }
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                running.store(false, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }

                    match tokio::time::timeout(
                        Duration::from_millis(50),
                        reader.read_packet(),
                    )
                    .await
                    {
                        Err(_) => {
                            // 超时后回到循环顶部，优先处理 Stop/控制命令，避免断开时长时间阻塞。
                            continue;
                        }
                        Ok(packet_result) => match packet_result {
                        Ok(Some(pkt)) => {
                            packet_total = packet_total.saturating_add(1);
                            if let Err(e) = pipeline.push_framed_packet(
                                pkt.data.to_vec(),
                                pkt.is_config,
                                pkt.is_keyframe,
                            ) {
                                warn!("push_framed_packet failed: {}", e);
                            }
                        }
                        Ok(None) => {
                            Self::push_event(
                                &session_id,
                                &events,
                                SessionEvent::Error {
                                    code: ErrorCode::DeviceDisconnected,
                                    message: "视频流结束".to_string(),
                                },
                            );
                            running.store(false, Ordering::Relaxed);
                        }
                        Err(e) => {
                            Self::push_event(
                                &session_id,
                                &events,
                                SessionEvent::Error {
                                    code: ErrorCode::DecodeFailed,
                                    message: format!("读取视频包失败: {}", e),
                                },
                            );
                            // 生产策略：视频读取出现协议/链路错误时立即结束本轮会话，
                            // 由上层按事件触发重连，避免后台线程在错误态空转占用 CPU。
                            running.store(false, Ordering::Relaxed);
                        }
                    },
                    }

                    while let Ok(evt) = event_rx.try_recv() {
                        match evt {
                            PipelineEvent::ReconfigureBegin {
                                generation: gen,
                                width,
                                height,
                            } => {
                                active_generation = gen;
                                generation.store(gen, Ordering::Relaxed);
                                debug!("runtime 重配开始: gen={} {}x{}", gen, width, height);
                            }
                            PipelineEvent::ResolutionChanged {
                                generation: gen,
                                width,
                                height,
                            }
                            | PipelineEvent::ReconfigureReady {
                                generation: gen,
                                width,
                                height,
                            } => {
                                active_generation = gen;
                                generation.store(gen, Ordering::Relaxed);
                                // 不在这里提前更新 last_size。
                                // 必须等待首个解码帧到来时再基于真实 frame(handle/size/gen) 触发
                                // ResolutionChanged，否则会吞掉旋转后的首个分辨率事件。
                                let _ = (width, height);
                            }
                        }
                    }

                    while let Ok(frame) = decoded_rx.try_recv() {
                        match frame {
                            DecodedFrame::GpuShared {
                                handle,
                                width,
                                height,
                                pts,
                            } => {
                                if matches!(render_pipeline_mode, RenderPipelineMode::CpuPixelBufferV2) {
                                    // V2 纯 CPU 链路下不应收到 GPU 句柄帧。
                                    warn!(
                                        "[API运行时] V2 链路收到 GPU 帧，已丢弃: handle={} size={}x{}",
                                        handle, width, height
                                    );
                                    continue;
                                }
                                let frame_generation = active_generation;
                                if handle <= 0 || width == 0 || height == 0 {
                                    let dropped = flow_monitor
                                        .invalid_frame_dropped
                                        .fetch_add(1, Ordering::Relaxed)
                                        + 1;
                                    if dropped % 120 == 1 {
                                        warn!(
                                            "[API运行时] 丢弃无效GPU帧: handle={} size={}x{} gen={} dropped={}",
                                            handle, width, height, frame_generation, dropped
                                        );
                                    }
                                    continue;
                                }
                                if frame_generation < active_generation {
                                    let dropped = flow_monitor
                                        .stale_generation_dropped
                                        .fetch_add(1, Ordering::Relaxed)
                                        + 1;
                                    if dropped % 120 == 1 {
                                        warn!(
                                            "[API运行时] 丢弃过期代际GPU帧: frame_gen={} active_gen={} dropped={}",
                                            frame_generation, active_generation, dropped
                                        );
                                    }
                                    continue;
                                }
                                Self::push_frame(
                                    &frames,
                                    &flow_monitor,
                                    TextureFrame {
                                        handle,
                                        width,
                                        height,
                                        generation: frame_generation,
                                        pts,
                                    },
                                );
                                // V1 回调驱动路径：
                                // - Rust 只推送 handle/size/gen 元信息；
                                // - Runner 侧更新 descriptor 并 markFrameAvailable；
                                // - Dart 不再以 8ms 轮询逐帧拉取 TextureFrame。
                                flutter_callback_register::notify_v1_frame(
                                    handle,
                                    width,
                                    height,
                                    frame_generation,
                                    pts,
                                );
                                // 双缓冲下 handle 会按帧交替变化，不应把它当作分辨率变更信号。
                                if (width, height) != last_size
                                    || frame_generation != active_generation
                                {
                                    active_generation = frame_generation;
                                    generation.store(frame_generation, Ordering::Relaxed);
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
                            }
                            DecodedFrame::CpuBgra(frame) => {
                                if matches!(render_pipeline_mode, RenderPipelineMode::Original) {
                                    // Original 链路下不应收到 CPU 帧。
                                    warn!(
                                        "[API运行时] Original 链路收到 CPU 帧，已丢弃: size={}x{}",
                                        frame.width, frame.height
                                    );
                                    continue;
                                }
                                let frame_generation = active_generation;
                                if frame.width == 0 || frame.height == 0 {
                                    let dropped = flow_monitor
                                        .invalid_frame_dropped
                                        .fetch_add(1, Ordering::Relaxed)
                                        + 1;
                                    if dropped % 120 == 1 {
                                        warn!(
                                            "[API运行时] 丢弃无效CPU帧: size={}x{} gen={} dropped={}",
                                            frame.width, frame.height, frame_generation, dropped
                                        );
                                    }
                                    continue;
                                }
                                if frame_generation < active_generation {
                                    let dropped = flow_monitor
                                        .stale_generation_dropped
                                        .fetch_add(1, Ordering::Relaxed)
                                        + 1;
                                    if dropped % 120 == 1 {
                                        warn!(
                                            "[API运行时] 丢弃过期代际CPU帧: frame_gen={} active_gen={} dropped={}",
                                            frame_generation, active_generation, dropped
                                        );
                                    }
                                    continue;
                                }
                                let frame_id = flutter_callback_register::next_frame_id();
                                // V2: 统一向 Runner 推送 RGBA，避免 C++ 侧逐像素 BGRA->RGBA 转换。
                                let rgba = Self::bgra_to_rgba(&frame.data);
                                flutter_callback_register::notify_v2_frame_raw(
                                    frame_id,
                                    &rgba,
                                    frame.width,
                                    frame.height,
                                    frame.width.saturating_mul(4),
                                    flutter_callback_register::PIXEL_FORMAT_RGBA32,
                                    frame_generation,
                                    frame.pts,
                                );
                                Self::push_frame(
                                    &frames,
                                    &flow_monitor,
                                    TextureFrame {
                                        // V2: handle 字段承载 frame_id（非共享句柄）。
                                        handle: 0,
                                        width: frame.width,
                                        height: frame.height,
                                        generation: frame_generation,
                                        pts: frame.pts,
                                    },
                                );
                                if (frame.width, frame.height) != last_size
                                    || frame_generation != active_generation
                                {
                                    active_generation = frame_generation;
                                    generation.store(frame_generation, Ordering::Relaxed);
                                    Self::push_event(
                                        &session_id,
                                        &events,
                                        SessionEvent::ResolutionChanged {
                                            width: frame.width,
                                            height: frame.height,
                                            // V2: new_handle 字段承载 frame_id。
                                            new_handle: 0,
                                            generation: frame_generation,
                                        },
                                    );
                                    last_size = (frame.width, frame.height);
                                }
                            }
                        }
                        fps_frames = fps_frames.saturating_add(1);
                        Self::update_stats(&stats, |s| {
                            s.total_frames = s.total_frames.saturating_add(1);
                        });
                    }

                    let snap = pipeline.stats();
                    // 先消费“已重同步”信号，用于硬解恢复窗口判定。
                    if snap.resync_signals > handled_resync_signals {
                        let delta = snap.resync_signals - handled_resync_signals;
                        handled_resync_signals = snap.resync_signals;
                        if let Some(state) = hw_recovery.take() {
                            let cost_ms = state.start_at.elapsed().as_millis();
                            warn!(
                                "硬解重同步成功: fail_signal={} resync_delta={} cost={}ms",
                                state.fail_signal, delta, cost_ms
                            );
                        }
                    }

                    if snap.need_idr_signals > handled_need_idr_signals {
                        let delta = snap.need_idr_signals - handled_need_idr_signals;
                        handled_need_idr_signals = snap.need_idr_signals;
                        if matches!(decoder_mode, DecoderMode::PreferHardware | DecoderMode::ForceHardware) {
                            // 硬解策略（生产）：先同会话内请求 IDR + 短窗口等待，不立刻整会话重连。
                            if hw_recovery.is_none() {
                                let now = Instant::now();
                                let state = HwRecoveryState {
                                    fail_signal: snap.need_idr_signals,
                                    start_at: now,
                                    deadline_at: now + Duration::from_millis(HW_RECOVERY_WAIT_MS),
                                };
                                warn!(
                                    "硬解解码失败，进入重同步窗口: fail_signal={} delta={} wait={}ms",
                                    state.fail_signal, delta, HW_RECOVERY_WAIT_MS
                                );
                                if let Err(e) = control.send_reset_video().await {
                                    warn!("硬解自动请求IDR失败: {}", e);
                                }
                                hw_recovery = Some(state);
                            } else {
                                warn!(
                                    "硬解重同步窗口内再次失败: fail_signal={} delta={}",
                                    snap.need_idr_signals, delta
                                );
                            }
                        } else {
                            warn!(
                                "decoder requested IDR after failure, signals_delta={}",
                                delta
                            );
                            if let Err(e) = control.send_reset_video().await {
                                warn!("auto request_idr(send_reset_video) failed: {}", e);
                            }
                        }
                    }

                    // 硬解重同步窗口超时：升级为整会话重连（快速失败）。
                    if let Some(state) = hw_recovery {
                        if Instant::now() >= state.deadline_at {
                            warn!(
                                "硬解重同步超时，触发整会话重连: fail_signal={} waited={}ms",
                                state.fail_signal,
                                state.start_at.elapsed().as_millis()
                            );
                            Self::push_event(&session_id, &events, SessionEvent::Reconnecting);
                            running.store(false, Ordering::Relaxed);
                            continue;
                        }
                    }
                    Self::update_stats(&stats, |s| {
                        s.decode_latency_ms = snap.last_decode_ms as u32;
                        s.upload_latency_ms = snap.last_upload_ms as u32;
                        s.dropped_frames = snap.dropped_frames;
                    });

                    if fps_tick.elapsed() >= Duration::from_secs(1) {
                        let secs = fps_tick.elapsed().as_secs_f64().max(0.001);
                        let fps = fps_frames as f64 / secs;
                        // let snap = pipeline.stats();
                        // let enqueued_total = flow_monitor.enqueued_total.load(Ordering::Relaxed);
                        // let replaced_same_gen = flow_monitor
                        //     .replaced_same_generation
                        //     .load(Ordering::Relaxed);
                        // let queue_trim_dropped =
                        //     flow_monitor.queue_trim_dropped.load(Ordering::Relaxed);
                        // let invalid_dropped =
                        //     flow_monitor.invalid_frame_dropped.load(Ordering::Relaxed);
                        // let stale_gen_dropped = flow_monitor
                        //     .stale_generation_dropped
                        //     .load(Ordering::Relaxed);
                        // let poll_calls = flow_monitor.polled_total.load(Ordering::Relaxed);
                        // let poll_drop_stale =
                        //     flow_monitor.polled_stale_dropped.load(Ordering::Relaxed);
                        // let pkt_delta = packet_total.saturating_sub(packet_delta_base);
                        // let decoded_delta = snap.decoded_frames.saturating_sub(frame_delta_base);
                        
                        // packet_delta_base = packet_total;
                        // frame_delta_base = snap.decoded_frames;
                        fps_frames = 0;
                        fps_tick = Instant::now();
                        Self::update_stats(&stats, |s| s.fps = fps);
                        let tx_full = decoded_tx_full_count.load(Ordering::Relaxed);
                        if tx_full > 0 {
                            debug!("runtime handoff summary: decoded_tx_full_total={}", tx_full);
                        }
                    }
                }

                let _ = server.stop().await;
                Self::push_event(&session_id, &events, SessionEvent::Stopped);
                running.store(false, Ordering::Relaxed);
            });
        }));

        Ok(())
    }

    /// 停止 worker，并清空运行时缓存队列。
    fn stop(&mut self) -> Result<()> {
        if !self.running.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _ = self.send_runtime_cmd(RuntimeCommand::Stop);
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.cmd_tx = None;
        if let Ok(mut q) = self.frames.lock() {
            q.clear();
        }
        if let Ok(mut q) = self.events.lock() {
            q.clear();
        }
        Ok(())
    }

    /// 轮询最新可渲染帧。
    ///
    /// 策略：每次只返回“最后一帧”，并统计被覆盖/丢弃的历史帧。
    fn poll_texture_frames(&mut self) -> Result<Vec<TextureFrame>> {
        // 生产策略：轮询接口只返回“当前最新可渲染帧”，彻底避免积压扩散到 Flutter UI 线程。
        let mut out = Vec::with_capacity(1);
        self.flow_monitor
            .polled_total
            .fetch_add(1, Ordering::Relaxed);
        if let Ok(mut q) = self.frames.lock() {
            let mut latest: Option<TextureFrame> = None;
            let mut popped: u64 = 0;
            while let Some(frame) = q.pop_front() {
                latest = Some(frame);
                popped = popped.saturating_add(1);
            }
            if popped > 1 {
                self.flow_monitor
                    .polled_stale_dropped
                    .fetch_add(popped - 1, Ordering::Relaxed);
                let now_ms = Self::now_millis();
                let last_warn_ms = self
                    .flow_monitor
                    .last_consumer_warn_ms
                    .load(Ordering::Relaxed);
                if now_ms.saturating_sub(last_warn_ms) >= 3000 {
                    self.flow_monitor
                        .last_consumer_warn_ms
                        .store(now_ms, Ordering::Relaxed);
                    warn!(
                        "[API运行时] Flutter消费滞后，单次轮询丢弃历史帧={}",
                        popped - 1
                    );
                }
            }
            if let Some(frame) = latest {
                self.flow_monitor
                    .polled_latest_returned
                    .fetch_add(1, Ordering::Relaxed);
                out.push(frame);
            }
        }
        Ok(out)
    }

    /// 轮询并清空当前事件队列。
    fn poll_session_events(&mut self) -> Result<Vec<SessionEvent>> {
        let mut out = Vec::new();
        if let Ok(mut q) = self.events.lock() {
            while let Some(event) = q.pop_front() {
                out.push(event);
            }
        }
        Ok(out)
    }

    /// 获取当前统计快照（锁失败时返回零值兜底）。
    fn stats(&self) -> SessionStats {
        self.stats
            .lock()
            .map(|s| s.clone())
            .unwrap_or(SessionStats {
                fps: 0.0,
                decode_latency_ms: 0,
                upload_latency_ms: 0,
                total_frames: 0,
                dropped_frames: 0,
            })
    }

    /// 返回运行态标记。
    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// 下发方向切换命令到 worker。
    fn set_orientation_mode(&mut self, mode: OrientationMode) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::SetOrientation(mode))
    }

    /// 下发触摸事件命令到 worker。
    fn send_touch(&mut self, event: TouchEvent) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::Touch(event))
    }

    /// 下发按键事件命令到 worker。
    fn send_key(&mut self, event: KeyEvent) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::Key(event))
    }

    /// 下发滚动事件命令到 worker。
    fn send_scroll(&mut self, event: ScrollEvent) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::Scroll(event))
    }

    /// 下发文本输入命令到 worker。
    fn send_text(&mut self, text: String) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::Text(text))
    }

    /// 下发系统按键命令到 worker。
    fn send_system_key(&mut self, key: SystemKey) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::SystemKey(key))
    }

    /// 下发剪贴板命令到 worker。
    fn set_clipboard(&mut self, text: String, paste: bool) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::Clipboard { text, paste })
    }

    /// 下发 IDR 请求命令到 worker。
    fn request_idr(&mut self) -> Result<()> {
        self.send_runtime_cmd(RuntimeCommand::RequestIdr)
    }
}
