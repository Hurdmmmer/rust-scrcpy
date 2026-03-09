use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::flutter_callback_register;
use crate::gh_common::{Result, ScrcpyError};
use crate::gh_common::model::{ErrorCode, SessionEvent, SessionStats};
use crate::scrcpy::decode_core::{DecodedFrame, PipelineStats};
use crate::scrcpy::client::scrcpy_conn::ScrcpyConnect;
use crate::scrcpy::client::scrcpy_control::{KeyEvent, ScrollEvent, TouchEvent};
use crate::scrcpy::client::ScrcpyClient;
use crate::scrcpy::runtime::scrcpy_decode_pipeline::{
    DecodeFrame, ScrcpyDecodeConfig, ScrcpyDecodeEvent, ScrcpyDecodePipeline,
};
use crate::scrcpy::session::SessionManager;
use tracing::{debug, info, warn};

/// Runtime 侧会话事件队列上限。
const RUNTIME_EVENT_QUEUE_LIMIT: usize = 512;
/// Runtime 侧原始解码事件队列上限。
const RUNTIME_DECODE_EVENT_QUEUE_LIMIT: usize = 256;
/// Runtime 侧帧缓冲上限（低延迟：仅保留最新少量帧）。
const RUNTIME_FRAME_QUEUE_LIMIT: usize = 2;

/// 单会话运行时性能状态。
///
/// 用途：
/// - 维护 FPS 滚动窗口；
/// - 跟踪当前活跃代际和分辨率，避免重复上报。
#[derive(Debug, Clone)]
struct RuntimePerfState {
    fps_tick: Instant,
    fps_frames: u64,
    active_generation: u64,
    last_size: (u32, u32),
}

impl RuntimePerfState {
    fn new() -> Self {
        Self {
            fps_tick: Instant::now(),
            fps_frames: 0,
            active_generation: 1,
            last_size: (0, 0),
        }
    }
}

/// Scrcpy 核心运行时。
///
/// 运行时层职责：
/// - 统一管理会话生命周期（start/stop）；
/// - 保持“会话表 + 解码管道表 + 事件/帧队列”同一 session_id 下的一致性；
/// - 路由控制指令到会话；
/// - 聚合 Session/Decode 事件并转发 callback；
/// - 维护会话级统计信息。
///
/// 非职责：
/// - 不直接处理底层建链细节（由 `ScrcpyClient` 负责）；
/// - 不实现会话容器细节（由 `SessionManager` 负责）。
pub struct ScrcpyCoreRuntime {
    /// 底层客户端：负责建链和断链。
    client: ScrcpyClient,
    /// 会话表：管理已连接的会话对象。
    session_manager: SessionManager,
    /// 解码管道表：每个会话对应一个独立解码管道。
    decode_pipelines: HashMap<String, ScrcpyDecodePipeline>,
    /// 会话事件队列表：每个会话一个最终事件出口。
    session_events: HashMap<String, VecDeque<SessionEvent>>,
    /// 原始解码事件队列表：用于调试诊断。
    decode_events: HashMap<String, VecDeque<ScrcpyDecodeEvent>>,
    /// Runtime 帧队列：供上层轮询（低延迟最新帧语义）。
    decoded_frames: HashMap<String, VecDeque<DecodeFrame>>,
    /// 会话统计快照。
    session_stats: HashMap<String, SessionStats>,
    /// 会话性能运行态。
    perf_states: HashMap<String, RuntimePerfState>,
    /// 解码配置模板：用于后续新会话初始化。
    decode_config: ScrcpyDecodeConfig,
}

impl ScrcpyCoreRuntime {
    /// 创建运行时实例。
    pub fn new(client: ScrcpyClient) -> Self {
        info!("[核心运行时] 初始化完成");
        Self {
            client,
            session_manager: SessionManager::new(),
            decode_pipelines: HashMap::new(),
            session_events: HashMap::new(),
            decode_events: HashMap::new(),
            decoded_frames: HashMap::new(),
            session_stats: HashMap::new(),
            perf_states: HashMap::new(),
            decode_config: ScrcpyDecodeConfig::default(),
        }
    }

    /// 设置解码管道配置（仅对后续新会话生效）。
    ///
    /// 已运行会话的解码管道不会被在线修改，避免运行时状态不一致。
    pub fn set_decode_config(&mut self, cfg: ScrcpyDecodeConfig) {
        info!("[核心运行时] 更新解码配置: mode={:?}", cfg.decoder_mode);
        self.decode_config = cfg;
    }

    /// 将 BGRA 转为 RGBA（供 V2 回调路径使用）。
    fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(src.len());
        for px in src.chunks_exact(4) {
            out.push(px[2]);
            out.push(px[1]);
            out.push(px[0]);
            out.push(px[3]);
        }
        out
    }

    /// 写入 runtime 会话事件队列，并立即转发到 callback 注册层。
    fn push_runtime_event(&mut self, session_id: &str, event: SessionEvent) {
        let q = self
            .session_events
            .entry(session_id.to_string())
            .or_insert_with(|| VecDeque::with_capacity(RUNTIME_EVENT_QUEUE_LIMIT));
        q.push_back(event.clone());
        while q.len() > RUNTIME_EVENT_QUEUE_LIMIT {
            let _ = q.pop_front();
        }

        match serde_json::to_vec(&event) {
            Ok(payload) => {
                // 注意：这里只调用既有 callback 转发函数，不修改 callback ABI。
                flutter_callback_register::notify_session_event(session_id, &payload);
            }
            Err(e) => {
                warn!("[核心运行时] SessionEvent 序列化失败: session_id={}, err={}", session_id, e);
            }
        }
    }

    /// 写入 runtime 原始解码事件队列。
    fn push_decode_event(&mut self, session_id: &str, event: ScrcpyDecodeEvent) {
        let q = self
            .decode_events
            .entry(session_id.to_string())
            .or_insert_with(|| VecDeque::with_capacity(RUNTIME_DECODE_EVENT_QUEUE_LIMIT));
        q.push_back(event);
        while q.len() > RUNTIME_DECODE_EVENT_QUEUE_LIMIT {
            let _ = q.pop_front();
        }
    }

    /// 写入 runtime 帧队列（最新帧策略）。
    fn push_runtime_frame(&mut self, session_id: &str, frame: DecodeFrame) {
        let q = self
            .decoded_frames
            .entry(session_id.to_string())
            .or_insert_with(|| VecDeque::with_capacity(RUNTIME_FRAME_QUEUE_LIMIT));

        if let Some(back) = q.back_mut() {
            if back.generation == frame.generation {
                // 同代际仅保留最新帧，避免 UI 处理历史帧。
                *back = frame;
                return;
            }
        }

        q.push_back(frame);
        while q.len() > RUNTIME_FRAME_QUEUE_LIMIT {
            let _ = q.pop_front();
        }
    }

    /// 统计快照初始化。
    fn default_stats() -> SessionStats {
        SessionStats {
            fps: 0.0,
            decode_latency_ms: 0,
            upload_latency_ms: 0,
            total_frames: 0,
            dropped_frames: 0,
        }
    }

    /// 将解码事件映射为对外 `SessionEvent`。
    fn map_decode_event(event: &ScrcpyDecodeEvent) -> Option<SessionEvent> {
        match event {
            ScrcpyDecodeEvent::DecoderStarted { .. } => None,
            ScrcpyDecodeEvent::DecoderDegraded { from, to, reason } => Some(SessionEvent::Error {
                code: ErrorCode::DecodeFailed,
                message: format!("decoder degraded: {:?} -> {:?}, reason={}", from, to, reason),
            }),
            ScrcpyDecodeEvent::ResolutionChanged {
                generation,
                width,
                height,
            } => Some(SessionEvent::ResolutionChanged {
                width: *width,
                height: *height,
                // 该事件来自解码层，尚未绑定具体 GPU 句柄，先置 0。
                new_handle: 0,
                generation: *generation,
            }),
            ScrcpyDecodeEvent::ReconnectRequired { .. } => Some(SessionEvent::Reconnecting),
            ScrcpyDecodeEvent::Warning { message } => Some(SessionEvent::Error {
                code: ErrorCode::Internal,
                message: message.clone(),
            }),
        }
    }

    /// 处理新解码帧：
    /// - 代际过滤；
    /// - 回调转发（V1/V2）；
    /// - 分辨率变更事件；
    /// - FPS/统计更新。
    fn process_new_frames(&mut self, session_id: &str, frames: Vec<DecodeFrame>) {
        if frames.is_empty() {
            return;
        }

        // 先拷贝运行态快照，避免持有 map 可变借用时再次调用 `self.*` 导致借用冲突。
        let mut perf = self
            .perf_states
            .get(session_id)
            .cloned()
            .unwrap_or_else(RuntimePerfState::new);
        let mut stats = self
            .session_stats
            .get(session_id)
            .cloned()
            .unwrap_or_else(Self::default_stats);

        for frame in frames {
            if frame.generation < perf.active_generation {
                // 丢弃过期代际帧，避免旧链路脏数据污染当前渲染。
                continue;
            }

            let (width, height, pts, handle_for_event) = match &frame.frame {
                DecodedFrame::GpuShared {
                    handle,
                    width,
                    height,
                    pts,
                } => {
                    // V1 回调：共享句柄元信息。
                    flutter_callback_register::notify_v1_frame(
                        *handle,
                        *width,
                        *height,
                        frame.generation,
                        *pts,
                    );
                    (*width, *height, *pts, *handle)
                }
                DecodedFrame::CpuBgra(cpu) => {
                    // V2 回调：CPU 像素数据。
                    let frame_id = flutter_callback_register::next_frame_id();
                    let rgba = Self::bgra_to_rgba(&cpu.data);
                    flutter_callback_register::notify_v2_frame_raw(
                        frame_id,
                        &rgba,
                        cpu.width,
                        cpu.height,
                        cpu.width.saturating_mul(4),
                        flutter_callback_register::PIXEL_FORMAT_RGBA32,
                        frame.generation,
                        cpu.pts,
                    );
                    (cpu.width, cpu.height, cpu.pts, 0)
                }
            };

            // 分辨率或代际变化时上报事件。
            if (width, height) != perf.last_size || frame.generation != perf.active_generation {
                perf.active_generation = frame.generation;
                perf.last_size = (width, height);
                self.push_runtime_event(
                    session_id,
                    SessionEvent::ResolutionChanged {
                        width,
                        height,
                        new_handle: handle_for_event,
                        generation: frame.generation,
                    },
                );
            }

            // 入 runtime 低延迟帧队列。
            self.push_runtime_frame(session_id, frame);

            // 更新统计。
            stats.total_frames = stats.total_frames.saturating_add(1);
            perf.fps_frames = perf.fps_frames.saturating_add(1);

            // 保留 pts 只用于回调链路，这里不单独存储。
            let _ = pts;
        }

        let elapsed = perf.fps_tick.elapsed();
        if elapsed.as_secs_f64() >= 1.0 {
            let fps = perf.fps_frames as f64 / elapsed.as_secs_f64().max(0.001);
            stats.fps = fps;
            perf.fps_frames = 0;
            perf.fps_tick = Instant::now();
        }

        self.perf_states.insert(session_id.to_string(), perf);
        self.session_stats.insert(session_id.to_string(), stats);
    }

    /// 聚合一次会话事件：
    /// - 先拉取 Session 本地事件；
    /// - 再拉取 Decode 事件并做模型映射。
    fn collect_session_events(&mut self, session_id: &str) {
        if let Some(session) = self.session_manager.get_mut(session_id) {
            for event in session.drain_events() {
                self.push_runtime_event(session_id, event);
            }
        }

        if let Some(pipeline) = self.decode_pipelines.get_mut(session_id) {
            for event in pipeline.drain_events() {
                self.push_decode_event(session_id, event.clone());
                if let Some(mapped) = Self::map_decode_event(&event) {
                    self.push_runtime_event(session_id, mapped);
                }
            }
        }
    }

    /// 使用当前 client 配置启动会话。
    pub async fn start(&mut self, session_id: String) -> Result<()> {
        debug!("[核心运行时] 请求启动会话: session_id={}", session_id);
        if self.session_manager.get(&session_id).is_some() {
            warn!("[核心运行时] 启动失败，会话已存在: session_id={}", session_id);
            return Err(ScrcpyError::Other(format!(
                "session already exists: {}",
                session_id
            )));
        }

        let conn = self.client.build_scrcpy_connect()?;
        self.start_with_conn(session_id, conn).await
    }

    /// 使用给定连接参数启动会话。
    pub async fn start_with_conn(&mut self, session_id: String, conn: ScrcpyConnect) -> Result<()> {
        debug!("[核心运行时] 使用外部连接对象启动会话: session_id={}", session_id);
        if self.session_manager.get(&session_id).is_some() {
            warn!("[核心运行时] 启动失败，会话已存在: session_id={}", session_id);
            return Err(ScrcpyError::Other(format!(
                "session already exists: {}",
                session_id
            )));
        }

        self.session_events
            .insert(session_id.clone(), VecDeque::with_capacity(RUNTIME_EVENT_QUEUE_LIMIT));
        self.decode_events
            .insert(session_id.clone(), VecDeque::with_capacity(RUNTIME_DECODE_EVENT_QUEUE_LIMIT));
        self.decoded_frames
            .insert(session_id.clone(), VecDeque::with_capacity(RUNTIME_FRAME_QUEUE_LIMIT));
        self.session_stats
            .insert(session_id.clone(), Self::default_stats());
        self.perf_states
            .insert(session_id.clone(), RuntimePerfState::new());
        self.push_runtime_event(&session_id, SessionEvent::Starting);

        let session = match self.client.start(conn).await {
            Ok(v) => v,
            Err(e) => {
                self.push_runtime_event(
                    &session_id,
                    SessionEvent::Error {
                        code: ErrorCode::Internal,
                        message: format!("start session failed: {}", e),
                    },
                );
                return Err(e);
            }
        };
        self.session_manager.insert(session_id.clone(), session)?;

        let pipeline = ScrcpyDecodePipeline::new(self.decode_config.clone())?;
        self.decode_pipelines.insert(session_id.clone(), pipeline);

        self.push_runtime_event(&session_id, SessionEvent::Running);
        info!("[核心运行时] 会话启动成功: session_id={}", session_id);
        Ok(())
    }

    /// 停止会话。
    pub async fn stop(&mut self, session_id: &str) -> Result<()> {
        debug!("[核心运行时] 请求停止会话: session_id={}", session_id);

        if let Some(mut pipeline) = self.decode_pipelines.remove(session_id) {
            pipeline.stop();
        }

        let mut session = self
            .session_manager
            .remove(session_id)
            .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;

        self.client.stop(&mut session).await?;
        self.push_runtime_event(session_id, SessionEvent::Stopped);

        // 停止后清理运行态缓存，避免后续复用 session_id 时读取旧数据。
        self.decoded_frames.remove(session_id);
        self.decode_events.remove(session_id);
        self.perf_states.remove(session_id);

        info!("[核心运行时] 会话停止成功: session_id={}", session_id);
        Ok(())
    }

    /// 拉取并清空会话事件队列（供 Flutter API 轮询）。
    pub fn drain_session_events(&mut self, session_id: &str) -> Result<Vec<SessionEvent>> {
        let q = self
            .session_events
            .get_mut(session_id)
            .ok_or_else(|| ScrcpyError::Other(format!("session events not found: {}", session_id)))?;
        let mut out = Vec::with_capacity(q.len());
        while let Some(event) = q.pop_front() {
            out.push(event);
        }
        Ok(out)
    }

    /// 执行一次解码泵送。
    pub async fn decode_pump_once(&mut self, session_id: &str) -> Result<bool> {
        let did_work = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            let pipeline = self
                .decode_pipelines
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("decode pipeline not found: {}", session_id)))?;
            pipeline.pump_once(session).await?
        };

        // 解码帧迁移到 runtime 队列，并转发回调。
        let fresh_frames = {
            let pipeline = self
                .decode_pipelines
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("decode pipeline not found: {}", session_id)))?;
            pipeline.drain_frames()
        };
        self.process_new_frames(session_id, fresh_frames);

        // 同步解码统计到会话统计。
        let snap = self
            .decode_pipelines
            .get(session_id)
            .ok_or_else(|| ScrcpyError::Other(format!("decode pipeline not found: {}", session_id)))?
            .stats();
        if let Some(s) = self.session_stats.get_mut(session_id) {
            s.decode_latency_ms = snap.last_decode_ms as u32;
            s.upload_latency_ms = snap.last_upload_ms as u32;
            s.dropped_frames = snap.dropped_frames;
        }

        self.collect_session_events(session_id);
        Ok(did_work)
    }

    /// 批量执行解码泵送，最多处理 `budget` 次。
    pub async fn decode_pump(&mut self, session_id: &str, budget: usize) -> Result<usize> {
        let mut worked = 0usize;
        for _ in 0..budget {
            if !self.decode_pump_once(session_id).await? {
                break;
            }
            worked += 1;
        }
        Ok(worked)
    }

    /// 获取解码统计快照。
    pub fn decode_stats(&self, session_id: &str) -> Result<PipelineStats> {
        let pipeline = self
            .decode_pipelines
            .get(session_id)
            .ok_or_else(|| ScrcpyError::Other(format!("decode pipeline not found: {}", session_id)))?;
        Ok(pipeline.stats())
    }

    /// 拉取并清空 runtime 帧队列。
    ///
    /// 策略：每次仅返回“最后一帧”，避免上层消费积压导致延迟扩散。
    pub fn drain_decoded_frames(&mut self, session_id: &str) -> Result<Vec<DecodeFrame>> {
        let q = self
            .decoded_frames
            .get_mut(session_id)
            .ok_or_else(|| ScrcpyError::Other(format!("decoded frames not found: {}", session_id)))?;
        let mut latest: Option<DecodeFrame> = None;
        while let Some(frame) = q.pop_front() {
            latest = Some(frame);
        }
        Ok(latest.into_iter().collect())
    }

    /// 拉取并清空原始解码事件（调试用途）。
    pub fn drain_decode_events(&mut self, session_id: &str) -> Result<Vec<ScrcpyDecodeEvent>> {
        let q = self
            .decode_events
            .get_mut(session_id)
            .ok_or_else(|| ScrcpyError::Other(format!("decode events not found: {}", session_id)))?;
        let mut out = Vec::with_capacity(q.len());
        while let Some(event) = q.pop_front() {
            out.push(event);
        }
        Ok(out)
    }

    /// 获取会话统计快照。
    pub fn session_stats(&self, session_id: &str) -> Result<SessionStats> {
        self.session_stats
            .get(session_id)
            .cloned()
            .ok_or_else(|| ScrcpyError::Other(format!("session stats not found: {}", session_id)))
    }

    /// 返回当前会话是否需要重连。
    pub fn decode_reconnect_required(&self, session_id: &str) -> Result<bool> {
        let pipeline = self
            .decode_pipelines
            .get(session_id)
            .ok_or_else(|| ScrcpyError::Other(format!("decode pipeline not found: {}", session_id)))?;
        Ok(pipeline.reconnect_required())
    }

    /// 获取会话数量。
    pub fn session_count(&self) -> usize {
        self.session_manager.len()
    }

    /// 列出全部会话 ID。
    pub fn list_session_ids(&self) -> Vec<String> {
        self.session_manager.list_session_ids()
    }

    /// 发送触摸事件。
    pub async fn send_touch(&mut self, session_id: &str, event: &TouchEvent) -> Result<()> {
        let ret = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            session.send_touch(event).await
        };
        self.collect_session_events(session_id);
        ret
    }

    /// 发送按键事件。
    pub async fn send_key(&mut self, session_id: &str, event: &KeyEvent) -> Result<()> {
        let ret = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            session.send_key(event).await
        };
        self.collect_session_events(session_id);
        ret
    }

    /// 发送滚轮事件。
    pub async fn send_scroll(&mut self, session_id: &str, event: &ScrollEvent) -> Result<()> {
        let ret = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            session.send_scroll(event).await
        };
        self.collect_session_events(session_id);
        ret
    }

    /// 发送文本输入。
    pub async fn send_text(&mut self, session_id: &str, text: &str) -> Result<()> {
        let ret = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            session.send_text(text).await
        };
        self.collect_session_events(session_id);
        ret
    }

    /// 设置设备剪贴板内容。
    pub async fn set_clipboard(&mut self, session_id: &str, text: &str, paste: bool) -> Result<()> {
        let ret = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            session.set_clipboard(text, paste).await
        };
        self.collect_session_events(session_id);
        ret
    }
    /// 请求关键帧。
    pub async fn request_idr(&mut self, session_id: &str) -> Result<()> {
        let ret = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            session.request_idr().await
        };
        self.collect_session_events(session_id);
        ret
    }

    /// 设置设备物理屏幕电源状态。
    ///
    /// 说明：
    /// - `on=false`：请求熄屏（仅关闭设备物理屏，不中断投屏）；
    /// - `on=true`：请求点亮屏幕。
    pub async fn set_display_power(&mut self, session_id: &str, on: bool) -> Result<()> {
        let ret = {
            let session = self
                .session_manager
                .get_mut(session_id)
                .ok_or_else(|| ScrcpyError::Other(format!("invalid session id: {}", session_id)))?;
            session.set_display_power(on).await
        };
        self.collect_session_events(session_id);
        ret
    }
}



