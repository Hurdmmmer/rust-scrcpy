use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::gh_common::model::DecoderMode;
use crate::gh_common::{Result, ScrcpyError};
use crate::scrcpy::decode_core::{
    DecodedFrame, DecoderOutputMode, DecoderPipeline, DecoderPreference, PipelineConfig,
    PipelineStats,
};
use crate::scrcpy::session::Session;

// 解码输出队列上限：仅保留最新若干帧，避免延迟累计。
const FRAME_QUEUE_LIMIT: usize = 2;
// 解码事件队列上限：防止异常风暴导致内存持续增长。
const EVENT_QUEUE_LIMIT: usize = 256;
// DecoderPipeline 回调到本层的中间队列容量。
const PIPELINE_HOOK_QUEUE: usize = 8;
// 硬解失败后的 IDR 恢复窗口（毫秒）。
const HW_RECOVERY_WAIT_MS: u64 = 220;

#[derive(Debug, Clone)]
/// 解码输出帧（带代际标签）。
///
/// 说明：
/// - generation 用于上层在重连/旋转后过滤旧数据；
/// - frame 为解码后的统一输出（GPU句柄或CPU像素）。
pub struct DecodeFrame {
    pub generation: u64,
    pub frame: DecodedFrame,
}

#[derive(Debug, Clone)]
/// 解码阶段事件。
///
/// 这些事件由解码管道内部产生，供 runtime 后续转发给上层。
pub enum ScrcpyDecodeEvent {
    DecoderStarted { decoder_mode: DecoderMode },
    DecoderDegraded { from: DecoderMode, to: DecoderMode, reason: String },
    ResolutionChanged { generation: u64, width: u32, height: u32 },
    ReconnectRequired { reason: String },
    Warning { message: String },
}

#[derive(Debug, Clone)]
/// 解码管道配置快照。
///
/// 设计约束：
/// - 配置在会话启动时固化；
/// - 运行中不直接修改，除非发生受控降级（硬解->软解）。
pub struct ScrcpyDecodeConfig {
    pub decoder_mode: DecoderMode,
    pub output_mode: DecoderOutputMode,
    pub frame_queue_capacity: usize,
    pub nal_queue_capacity: usize,
    pub drop_nal_on_full: bool,
}

impl Default for ScrcpyDecodeConfig {
    fn default() -> Self {
        Self {
            decoder_mode: DecoderMode::PreferHardware,
            output_mode: DecoderOutputMode::GpuShared,
            frame_queue_capacity: 2,
            nal_queue_capacity: 128,
            drop_nal_on_full: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct HwRecoveryState {
    fail_signal: u64,
    start_at: Instant,
    deadline_at: Instant,
}

/// Scrcpy 解码管道。
///
/// 关键策略：
/// - 硬解失败先请求 IDR，超时后自动降级软解；
/// - 分辨率变化触发代际提升并清理历史脏数据；
/// - 软解仍失败则标记需要重连，由上层 runtime 执行会话重建。
pub struct ScrcpyDecodePipeline {
    /// 本会话固化的解码配置。
    config: ScrcpyDecodeConfig,
    /// 当前实际生效的解码模式（可能因降级从硬解切到软解）。
    active_decoder_mode: DecoderMode,
    /// 代际号：用于上层过滤旋转/重建前的历史帧。
    generation: Arc<AtomicU64>,
    /// 底层解码流水线实例。
    pipeline: DecoderPipeline,
    /// 解码回调发送端：由 DecoderPipeline 回调线程写入。
    /// 解码回调接收端：由本对象在 pump 时消费。
    frame_rx: Receiver<DecodedFrame>,
    /// 输出帧缓冲（仅保留最新帧，避免延迟堆积）。
    frames: VecDeque<DecodeFrame>,
    /// 运行事件缓冲（供 runtime 拉取并上抛）。
    events: VecDeque<ScrcpyDecodeEvent>,
    /// 已处理的 need_idr 信号计数，避免重复触发恢复逻辑。
    handled_need_idr_signals: u64,
    /// 已处理的 resync 信号计数，用于确认恢复成功。
    handled_resync_signals: u64,
    /// 硬解恢复窗口状态（等待 IDR 生效）。
    hw_recovery: Option<HwRecoveryState>,
    /// 上一帧分辨率，用于检测旋转/分辨率切换。
    last_resolution: Option<(u32, u32)>,
    /// 是否需要上层执行重连。
    reconnect_required: bool,
}

impl ScrcpyDecodePipeline {
    /// 创建解码管道并固化本会话配置。
    ///
    /// 关键动作：
    /// - 初始化 DecoderPipeline；
    /// - 初始化代际和事件/帧队列；
    /// - 记录启动日志与 DecoderStarted 事件。
    pub fn new(config: ScrcpyDecodeConfig) -> Result<Self> {
        let (frame_tx, frame_rx) = sync_channel::<DecodedFrame>(PIPELINE_HOOK_QUEUE);
        let mut this = Self {
            config: config.clone(),
            active_decoder_mode: config.decoder_mode,
            generation: Arc::new(AtomicU64::new(1)),
            pipeline: Self::build_pipeline(&config, frame_tx.clone())?,
            frame_rx,
            frames: VecDeque::with_capacity(FRAME_QUEUE_LIMIT),
            events: VecDeque::with_capacity(EVENT_QUEUE_LIMIT),
            handled_need_idr_signals: 0,
            handled_resync_signals: 0,
            hw_recovery: None,
            last_resolution: None,
            reconnect_required: false,
        };

        this.push_event(ScrcpyDecodeEvent::DecoderStarted {
            decoder_mode: this.active_decoder_mode,
        });
        info!(
            "[解码管道] 初始化完成: mode={:?}, output={:?}, frame_q={}, nal_q={}, drop_nal_on_full={}",
            this.active_decoder_mode,
            this.config.output_mode,
            this.config.frame_queue_capacity,
            this.config.nal_queue_capacity,
            this.config.drop_nal_on_full
        );
        Ok(this)
    }

    /// 构建底层 DecoderPipeline。
    ///
    /// 说明：
    /// - 根据 decoder_mode 映射硬解/软解偏好；
    /// - 通过回调把解码结果送入本层同步队列；
    /// - 回调队列满时丢弃本次输出，保持低延迟。
    fn build_pipeline(config: &ScrcpyDecodeConfig, frame_tx: SyncSender<DecodedFrame>) -> Result<DecoderPipeline> {
        let preference = match config.decoder_mode {
            DecoderMode::PreferHardware => DecoderPreference::PreferHardware,
            DecoderMode::ForceHardware => DecoderPreference::ForceHardware,
            DecoderMode::ForceSoftware => DecoderPreference::ForceSoftware,
        };

        let pipeline_cfg = PipelineConfig {
            frame_queue_capacity: config.frame_queue_capacity,
            nal_queue_capacity: config.nal_queue_capacity,
            decoder_preference: preference,
            drop_nal_on_full: config.drop_nal_on_full,
            decoder_output_mode: config.output_mode,
        };

        DecoderPipeline::start(pipeline_cfg, move |frame| {
            match frame_tx.try_send(frame) {
                Ok(_) => Ok(()),
                Err(TrySendError::Full(_)) => Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    Err(ScrcpyError::Other("decode frame hook disconnected".to_string()))
                }
            }
        })
    }

    /// 推送解码事件到本地队列（环形截断）。
    fn push_event(&mut self, event: ScrcpyDecodeEvent) {
        self.events.push_back(event);
        while self.events.len() > EVENT_QUEUE_LIMIT {
            let _ = self.events.pop_front();
        }
    }

    /// 推送解码帧到本地队列（仅保留最新若干帧）。
    fn push_frame(&mut self, frame: DecodedFrame) {
        let gen = self.generation.load(Ordering::Relaxed);
        self.frames.push_back(DecodeFrame { generation: gen, frame });
        while self.frames.len() > FRAME_QUEUE_LIMIT {
            let _ = self.frames.pop_front();
        }
    }

    /// 提取解码帧分辨率，用于旋转/重配置判定。
    fn frame_size(frame: &DecodedFrame) -> (u32, u32) {
        match frame {
            DecodedFrame::GpuShared { width, height, .. } => (*width, *height),
            DecodedFrame::CpuBgra(b) => (b.width, b.height),
        }
    }

    /// 提升代际并清理历史数据。
    ///
    /// 该操作用于分辨率变化、解码器重建、重连等场景，
    /// 防止旧会话/旧分辨率帧污染新链路。
    fn bump_generation_and_clear_buffers(&mut self, reason: &str) {
        let next = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.frames.clear();
        loop {
            match self.frame_rx.try_recv() {
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        self.last_resolution = None;
        debug!("[解码管道] 清理历史脏数据并提升代际: generation={}, reason={}", next, reason);
    }

    /// 标记需要重连。
    ///
    /// 说明：
    /// - 对齐历史稳定策略：硬解恢复窗口超时后，优先整会话重连；
    /// - 避免在会话内强行降软解导致旋转后状态不一致。
    fn mark_reconnect_required(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if self.reconnect_required {
            return;
        }
        self.reconnect_required = true;
        self.push_event(ScrcpyDecodeEvent::ReconnectRequired {
            reason: reason.clone(),
        });
        warn!("[解码管道] 标记需要会话重连: {}", reason);
    }

    /// 消费解码统计并驱动恢复状态机。
    ///
    /// 处理逻辑：
    /// - resync_signals: 关闭硬解恢复窗口；
    /// - need_idr_signals: 触发 IDR 请求或重连标记；
    /// - 恢复窗口超时: 标记会话重连（对齐历史稳定行为）。
    async fn handle_pipeline_stats(&mut self, session: &mut Session, stats: PipelineStats) -> Result<()> {
        if stats.resync_signals > self.handled_resync_signals {
            self.handled_resync_signals = stats.resync_signals;
            if let Some(state) = self.hw_recovery.take() {
                let cost_ms = state.start_at.elapsed().as_millis();
                info!(
                    "[解码管道] 硬解重同步成功: fail_signal={}, cost={}ms",
                    state.fail_signal,
                    cost_ms
                );
            }
        }

        if stats.need_idr_signals > self.handled_need_idr_signals {
            self.handled_need_idr_signals = stats.need_idr_signals;
            let reason = format!("need_idr_signals={}", stats.need_idr_signals);

            match self.active_decoder_mode {
                DecoderMode::PreferHardware | DecoderMode::ForceHardware => {
                    if self.hw_recovery.is_none() {
                        let now = Instant::now();
                        self.hw_recovery = Some(HwRecoveryState {
                            fail_signal: stats.need_idr_signals,
                            start_at: now,
                            deadline_at: now + Duration::from_millis(HW_RECOVERY_WAIT_MS),
                        });
                        info!(
                            "[解码管道] 硬解失败，进入 IDR 恢复窗口: {}ms",
                            HW_RECOVERY_WAIT_MS
                        );
                        if let Err(e) = session.request_idr().await { warn!("[解码管道] 自动请求 IDR 失败: {}", e); }
                    }
                }
                DecoderMode::ForceSoftware => {
                    self.mark_reconnect_required(format!("software decode failed: {}", reason));



                }
            }
        }

        if let Some(state) = self.hw_recovery {
            if Instant::now() >= state.deadline_at {
                self.hw_recovery = None;
                self.mark_reconnect_required(format!("hardware recovery timeout: fail_signal={}, waited={}ms", state.fail_signal, state.start_at.elapsed().as_millis()));
            }
        }

        Ok(())
    }

    /// 执行一次解码泵送。
    ///
    /// 处理流程：
    /// 1. 从会话视频流读取一个分帧包；
    /// 2. 推入 DecoderPipeline；
    /// 3. 拉取解码输出并进行分辨率/代际处理；
    /// 4. 根据解码统计执行恢复/降级/重连判定。
    pub async fn pump_once(&mut self, session: &mut Session) -> Result<bool> {
        if self.reconnect_required {
            return Ok(false);
        }

        // 直接异步等待下一包数据，不再使用固定时间片轮询。
        // 停止响应由外层 worker 的 tokio::select! 抢占控制：
        // 当控制命令（如 Stop）先到达时，本次 pump future 会被取消并让出执行权。
        let packet_opt = {
            let reader = session.video_stream_mut()?;
            reader.read_packet().await?
        };

        let mut did_work = false;

        if let Some(packet) = packet_opt {
            did_work = true;
            self.pipeline.push_framed_packet(
                packet.data.to_vec(),
                packet.is_config,
                packet.is_keyframe,
            )?;
        }

        loop {
            match self.frame_rx.try_recv() {
                Ok(frame) => {
                    did_work = true;
                    let (w, h) = Self::frame_size(&frame);
                    if self.last_resolution != Some((w, h)) {
                        self.bump_generation_and_clear_buffers("resolution changed");
                        let gen = self.generation.load(Ordering::Relaxed);
                        self.push_event(ScrcpyDecodeEvent::ResolutionChanged {
                            generation: gen,
                            width: w,
                            height: h,
                        });
                        self.last_resolution = Some((w, h));
                    }
                    self.push_frame(frame);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.reconnect_required = true;
                    self.push_event(ScrcpyDecodeEvent::ReconnectRequired {
                        reason: "decode output disconnected".to_string(),
                    });
                    break;
                }
            }
        }

        let stats = self.pipeline.stats();
        self.handle_pipeline_stats(session, stats).await?;

        Ok(did_work)
    }

    /// 返回当前管道是否要求上层执行会话重连。
    pub fn reconnect_required(&self) -> bool {
        self.reconnect_required
    }

    /// 返回底层 DecoderPipeline 统计快照。
    pub fn stats(&self) -> PipelineStats {
        self.pipeline.stats()
    }

    /// 拉取并清空当前解码帧队列。
    pub fn drain_frames(&mut self) -> Vec<DecodeFrame> {
        let mut out = Vec::with_capacity(self.frames.len());
        while let Some(frame) = self.frames.pop_front() {
            out.push(frame);
        }
        out
    }

    /// 拉取并清空当前解码事件队列。
    pub fn drain_events(&mut self) -> Vec<ScrcpyDecodeEvent> {
        let mut out = Vec::with_capacity(self.events.len());
        while let Some(event) = self.events.pop_front() {
            out.push(event);
        }
        out
    }

    /// 停止解码管道并清理本地缓存。
    pub fn stop(&mut self) {
        info!("[解码管道] 停止");
        self.pipeline.stop();
        self.frames.clear();
        self.events.clear();
        self.hw_recovery = None;
    }
}










