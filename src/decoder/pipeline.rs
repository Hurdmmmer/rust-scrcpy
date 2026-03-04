use crate::decoder::{DecodedFrame, DecoderFactory, DecoderOutputMode, DecoderPreference};
use crate::error::{Result, ScrcpyError};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// 解码流水线配置。
///
/// - `frame_queue_capacity`：解码后帧队列容量，建议小值保证实时性；
/// - `nal_queue_capacity`：输入 NAL 缓冲容量，吸收网络抖动。
#[derive(Debug, Clone, Copy)]
pub struct PipelineConfig {
    pub frame_queue_capacity: usize,
    pub nal_queue_capacity: usize,
    /// 解码器选择策略（由代码配置决定，不依赖环境变量）。
    pub decoder_preference: DecoderPreference,
    /// 输入队列满时是否直接丢 NAL。
    ///
    /// - `false`：阻塞等待（默认，画质优先，避免花屏/彩条）；
    /// - `true`：低延迟优先，可能出现参考帧链断裂。
    pub drop_nal_on_full: bool,
    /// 解码输出模式：
    /// - `GpuShared`: 旧链路，输出共享纹理句柄；
    /// - `CpuBgra`: V2 链路，输出 CPU BGRA 帧。
    pub decoder_output_mode: DecoderOutputMode,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            frame_queue_capacity: 3,
            nal_queue_capacity: 128,
            decoder_preference: DecoderPreference::PreferHardware,
            drop_nal_on_full: false,
            decoder_output_mode: DecoderOutputMode::GpuShared,
        }
    }
}

/// 解码流水线统计信息快照。
#[derive(Debug, Clone, Copy, Default)]
pub struct PipelineStats {
    pub decoded_frames: u64,
    pub uploaded_frames: u64,
    pub dropped_frames: u64,
    pub dropped_nals: u64,
    pub last_decode_ms: u64,
    pub last_upload_ms: u64,
    pub need_idr_signals: u64,
    /// 解码器从 NeedIdr 状态恢复成功次数（收到可重同步的 IDR 并回到 Synced）。
    pub resync_signals: u64,
}

/// 兼容保留的流水线事件类型。
///
/// 当前解码流水线主实现未发出该事件；保留仅用于旧测试/示例编译兼容。
#[derive(Debug, Clone, Copy)]
pub enum PipelineEvent {
    ReconfigureBegin {
        generation: u64,
        width: u32,
        height: u32,
    },
    ResolutionChanged {
        generation: u64,
        width: u32,
        height: u32,
    },
    ReconfigureReady {
        generation: u64,
        width: u32,
        height: u32,
    },
}

#[derive(Default)]
struct PipelineStatsAtomic {
    decoded_frames: AtomicU64,
    uploaded_frames: AtomicU64,
    dropped_frames: AtomicU64,
    dropped_nals: AtomicU64,
    last_decode_ms: AtomicU64,
    last_upload_ms: AtomicU64,
    need_idr_signals: AtomicU64,
    resync_signals: AtomicU64,
}

impl PipelineStatsAtomic {
    fn snapshot(&self) -> PipelineStats {
        PipelineStats {
            decoded_frames: self.decoded_frames.load(Ordering::Relaxed),
            uploaded_frames: self.uploaded_frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            dropped_nals: self.dropped_nals.load(Ordering::Relaxed),
            last_decode_ms: self.last_decode_ms.load(Ordering::Relaxed),
            last_upload_ms: self.last_upload_ms.load(Ordering::Relaxed),
            need_idr_signals: self.need_idr_signals.load(Ordering::Relaxed),
            resync_signals: self.resync_signals.load(Ordering::Relaxed),
        }
    }
}

struct LatestFrameQueue<T> {
    inner: Mutex<LatestFrameQueueInner<T>>,
    cv: Condvar,
    capacity: usize,
}

struct LatestFrameQueueInner<T> {
    closed: bool,
    queue: VecDeque<T>,
}

/// 解码输入单元。
///
/// - `RawNal`：旧链路输入，单个不带起始码的 NAL；
/// - `FramedPacket`：scrcpy 分帧协议输入，已由服务端切好包边界。
enum VideoInput {
    RawNal(Vec<u8>),
    FramedPacket(FramedInputPacket),
}

/// 分帧协议输入包（来自 `send_frame_meta=true`）。
struct FramedInputPacket {
    data: Vec<u8>,
    is_config: bool,
    is_keyframe: bool,
}

/// H.264 访问单元(AU)合并器：
/// - 缓存 SPS/PPS（config NAL）；
/// - 依据 slice 起始信息聚合同一帧的多个 NAL；
/// - 仅在检测到下一帧边界时输出完整 AU（Annex-B）。
struct H264AuMerger {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    prefix_nals: Vec<Vec<u8>>,
    current_au_nals: Vec<Vec<u8>>,
    current_has_vcl: bool,
    current_has_idr: bool,
}

impl H264AuMerger {
    fn new() -> Self {
        Self {
            sps: None,
            pps: None,
            prefix_nals: Vec::new(),
            current_au_nals: Vec::new(),
            current_has_vcl: false,
            current_has_idr: false,
        }
    }

    #[inline]
    fn nal_type(nal: &[u8]) -> Option<u8> {
        nal.first().map(|b| b & 0x1F)
    }

    #[inline]
    fn append_annexb(dst: &mut Vec<u8>, nal: &[u8]) {
        dst.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        dst.extend_from_slice(nal);
    }

    fn remove_emulation_prevention(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(src.len());
        let mut i = 0usize;
        while i < src.len() {
            if i + 2 < src.len() && src[i] == 0 && src[i + 1] == 0 && src[i + 2] == 3 {
                out.push(0);
                out.push(0);
                i += 3;
            } else {
                out.push(src[i]);
                i += 1;
            }
        }
        out
    }

    fn read_ue(rbsp: &[u8], bit_pos: &mut usize) -> Option<u32> {
        let mut zeros = 0usize;
        while *bit_pos < rbsp.len() * 8 {
            let byte = rbsp[*bit_pos / 8];
            let shift = 7 - (*bit_pos % 8);
            let bit = (byte >> shift) & 1;
            *bit_pos += 1;
            if bit == 0 {
                zeros += 1;
            } else {
                break;
            }
            if zeros > 31 {
                return None;
            }
        }
        let mut suffix = 0u32;
        for _ in 0..zeros {
            if *bit_pos >= rbsp.len() * 8 {
                return None;
            }
            let byte = rbsp[*bit_pos / 8];
            let shift = 7 - (*bit_pos % 8);
            let bit = (byte >> shift) & 1;
            *bit_pos += 1;
            suffix = (suffix << 1) | (bit as u32);
        }
        Some(((1u32 << zeros) - 1) + suffix)
    }

    /// H.264 slice_header: first_mb_in_slice == 0 表示该 slice 是一帧起始。
    fn is_first_slice(nal: &[u8]) -> bool {
        if nal.len() < 2 {
            return false;
        }
        let rbsp = Self::remove_emulation_prevention(&nal[1..]);
        let mut bit_pos = 0usize;
        matches!(Self::read_ue(&rbsp, &mut bit_pos), Some(0))
    }

    fn build_packet_from_current(&mut self) -> Option<MergedAu> {
        if self.current_au_nals.is_empty() {
            return None;
        }
        let mut packet = Vec::with_capacity(4096);
        if let Some(sps) = &self.sps {
            Self::append_annexb(&mut packet, sps);
        }
        if let Some(pps) = &self.pps {
            Self::append_annexb(&mut packet, pps);
        }
        for n in &self.current_au_nals {
            Self::append_annexb(&mut packet, n);
        }
        self.current_au_nals.clear();
        self.current_has_vcl = false;
        let is_idr = self.current_has_idr;
        self.current_has_idr = false;
        Some(MergedAu {
            packet,
            is_idr,
            is_config: false,
        })
    }

    fn start_new_au_if_needed(&mut self) {
        if self.current_au_nals.is_empty() && !self.prefix_nals.is_empty() {
            self.current_au_nals.append(&mut self.prefix_nals);
        }
    }

    /// 输入单个不带起始码 NAL，遇到“下一帧边界”时输出上一帧完整 AU。
    fn merge_nal(&mut self, nal: &[u8]) -> Option<MergedAu> {
        let nal_type = Self::nal_type(nal)?;

        match nal_type {
            7 => {
                self.sps = Some(nal.to_vec());
                return None;
            }
            8 => {
                self.pps = Some(nal.to_vec());
                return None;
            }
            9 => {
                // AUD 到来时，如果已有当前 AU，则可安全输出。
                return self.build_packet_from_current();
            }
            6 => {
                // SEI 作为 prefix 信息，挂到下一帧。
                if self.current_au_nals.is_empty() {
                    self.prefix_nals.push(nal.to_vec());
                } else {
                    self.current_au_nals.push(nal.to_vec());
                }
                return None;
            }
            1 | 5 => {
                let first = Self::is_first_slice(nal);
                if self.current_has_vcl && first {
                    // 新帧起始，先输出上一帧。
                    let out = self.build_packet_from_current();
                    self.start_new_au_if_needed();
                    self.current_au_nals.push(nal.to_vec());
                    self.current_has_vcl = true;
                    self.current_has_idr = nal_type == 5;
                    return out;
                }

                self.start_new_au_if_needed();
                self.current_au_nals.push(nal.to_vec());
                self.current_has_vcl = true;
                self.current_has_idr |= nal_type == 5;
                return None;
            }
            _ => {
                // 其余 NAL（如 filler）并入当前 AU；若尚未开帧则作为 prefix。
                if self.current_au_nals.is_empty() {
                    self.prefix_nals.push(nal.to_vec());
                } else {
                    self.current_au_nals.push(nal.to_vec());
                }
                return None;
            }
        }
    }
}

/// AU 合并输出：包含可送解码器的 Annex-B 包以及关键帧标记。
struct MergedAu {
    packet: Vec<u8>,
    is_idr: bool,
    is_config: bool,
}

/// scrcpy 分帧协议的配置包合并器（语义对齐 packet_merger.c）。
///
/// 规则：
/// - 收到 config 包：缓存，且该包本身也下发给解码器；
/// - 收到 media 包：若有缓存 config，则 prepend 后输出并清空缓存；
/// - `is_keyframe` 直接沿用协议标志，避免在客户端重复猜测。
struct FramedPacketMerger {
    config: Option<Vec<u8>>,
}

impl FramedPacketMerger {
    fn new() -> Self {
        Self { config: None }
    }

    fn merge(&mut self, pkt: FramedInputPacket) -> Option<MergedAu> {
        if pkt.is_config {
            // 对齐 scrcpy packet_merger：
            // config 包本身需要下发给解码器，同时缓存一份用于“下一包 media prepend 一次”。
            self.config = Some(pkt.data.clone());
            return Some(MergedAu {
                packet: pkt.data,
                is_idr: false,
                is_config: true,
            });
        }

        let mut out = pkt.data;
        // 对齐 scrcpy packet_merger：
        // 仅当“紧邻在 config 之后的第一包 media”到来时 prepend，
        // 然后清空 pending config。
        if let Some(cfg) = self.config.take() {
            let mut merged = Vec::with_capacity(cfg.len() + out.len());
            merged.extend_from_slice(&cfg);
            merged.extend_from_slice(&out);
            out = merged;
        }

        Some(MergedAu {
            packet: out,
            is_idr: pkt.is_keyframe,
            is_config: false,
        })
    }
}

/// 解码同步状态：
/// - `NeedIdr`：处于失步状态，只接受 IDR 帧恢复；
/// - `Synced`：正常解码状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeSyncState {
    NeedIdr,
    Synced,
}

impl<T> LatestFrameQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LatestFrameQueueInner {
                closed: false,
                queue: VecDeque::with_capacity(capacity.max(1)),
            }),
            cv: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    /// 仅保留“最新帧”策略：
    /// 队列满时丢弃最旧帧，避免延迟不断累积。
    fn push_latest(&self, item: T) -> bool {
        let mut dropped = false;
        if let Ok(mut g) = self.inner.lock() {
            if g.closed {
                return false;
            }
            if g.queue.len() >= self.capacity {
                let _ = g.queue.pop_front();
                dropped = true;
            }
            g.queue.push_back(item);
            self.cv.notify_one();
        }
        dropped
    }

    /// 等待获取一帧（支持超时），用于上传线程低频轮询。
    fn pop_wait(&self, timeout: Duration) -> Option<T> {
        let mut g = self.inner.lock().ok()?;
        loop {
            if let Some(item) = g.queue.pop_front() {
                return Some(item);
            }
            if g.closed {
                return None;
            }
            let (guard, wait_res) = self.cv.wait_timeout(g, timeout).ok()?;
            g = guard;
            if wait_res.timed_out() && g.queue.is_empty() {
                return None;
            }
        }
    }

    fn close(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.closed = true;
            self.cv.notify_all();
        }
    }
}

pub struct DecoderPipeline {
    input_tx: SyncSender<VideoInput>,
    drop_nal_on_full: bool,
    stop: Arc<AtomicBool>,
    frame_queue: Arc<LatestFrameQueue<DecodedFrame>>,
    stats: Arc<PipelineStatsAtomic>,
    decode_join: Option<JoinHandle<()>>,
    upload_join: Option<JoinHandle<()>>,
}

impl DecoderPipeline {
    /// 启动双线程流水线：
    /// - 解码线程：NAL -> `DecodedFrame`（CPU/GPU 统一输出）；
    /// - 上传线程：消费 `DecodedFrame` 并调用上层回调。
    pub fn start<F>(config: PipelineConfig, mut on_frame: F) -> Result<Self>
    where
        F: FnMut(DecodedFrame) -> Result<()> + Send + 'static,
    {
        let (input_tx, input_rx) = sync_channel::<VideoInput>(config.nal_queue_capacity.max(1));
        let stop = Arc::new(AtomicBool::new(false));
        let frame_queue = Arc::new(LatestFrameQueue::new(config.frame_queue_capacity));
        let stats = Arc::new(PipelineStatsAtomic::default());

        let stop_decode = Arc::clone(&stop);
        let stop_upload = Arc::clone(&stop);
        let queue_for_decode = Arc::clone(&frame_queue);
        let queue_for_upload = Arc::clone(&frame_queue);
        let stats_decode = Arc::clone(&stats);
        let stats_upload = Arc::clone(&stats);

        let decode_join = thread::Builder::new()
            .name("decode-thread".to_string())
            .spawn(move || {
                decode_loop(
                    input_rx,
                    stop_decode,
                    queue_for_decode,
                    stats_decode,
                    config.decoder_preference,
                    config.decoder_output_mode,
                )
            })
            .map_err(|e| ScrcpyError::Other(format!("create decode thread failed: {}", e)))?;

        let upload_join = thread::Builder::new()
            .name("upload-thread".to_string())
            .spawn(move || upload_loop(&mut on_frame, stop_upload, queue_for_upload, stats_upload))
            .map_err(|e| ScrcpyError::Other(format!("create upload thread failed: {}", e)))?;

        Ok(Self {
            input_tx,
            drop_nal_on_full: config.drop_nal_on_full,
            stop,
            frame_queue,
            stats,
            decode_join: Some(decode_join),
            upload_join: Some(upload_join),
        })
    }

    /// 兼容旧接口：保留事件回调签名，但当前实现不产出事件。
    pub fn start_with_events<F, E>(
        config: PipelineConfig,
        on_frame: F,
        _on_event: E,
    ) -> Result<Self>
    where
        F: FnMut(DecodedFrame) -> Result<()> + Send + 'static,
        E: FnMut(PipelineEvent) -> Result<()> + Send + 'static,
    {
        Self::start(config, on_frame)
    }

    /// 投喂单个 NAL。
    ///
    /// 若输入队列拥塞，当前实现选择“丢弃新 NAL 并计数”，
    /// 避免阻塞调用方线程。
    pub fn push_nal(&self, nal: Vec<u8>) -> Result<()> {
        self.push_input(VideoInput::RawNal(nal))
    }

    /// 投喂 scrcpy 分帧协议包。
    ///
    /// 注意：
    /// - `data` 必须是“一个完整包”的 payload；
    /// - `is_config`/`is_keyframe` 来自协议头标志位；
    /// - 不需要再由客户端手动拆 AU。
    pub fn push_framed_packet(
        &self,
        data: Vec<u8>,
        is_config: bool,
        is_keyframe: bool,
    ) -> Result<()> {
        self.push_input(VideoInput::FramedPacket(FramedInputPacket {
            data,
            is_config,
            is_keyframe,
        }))
    }

    fn push_input(&self, input: VideoInput) -> Result<()> {
        if !self.drop_nal_on_full {
            // 画质优先模式：对编码包施加背压，不做超时丢弃。
            // 说明：
            // - 编码包丢失会破坏 H.264 参考链，比短暂阻塞更容易导致花屏；
            // - 解码线程与调用线程分离，阻塞 send 只会在拥塞时触发背压。
            self.input_tx.send(input).map_err(|_| {
                ScrcpyError::Other("pipeline stopped, unable to push input".to_string())
            })?;
            return Ok(());
        }

        match self.input_tx.try_send(input) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.stats.dropped_nals.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(ScrcpyError::Other(
                "pipeline stopped, unable to push input".to_string(),
            )),
        }
    }

    pub fn stats(&self) -> PipelineStats {
        self.stats.snapshot()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.frame_queue.close();

        if let Some(j) = self.decode_join.take() {
            let _ = j.join();
        }
        if let Some(j) = self.upload_join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for DecoderPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 测试辅助：将一组裸 H264 NAL 按访问单元规则合并。
///
/// 返回值为 `(annexb_packet, is_idr)` 列表。
pub fn merge_h264_access_units_for_test(nals: &[Vec<u8>]) -> Vec<(Vec<u8>, bool)> {
    let mut merger = H264AuMerger::new();
    let mut out = Vec::new();
    for nal in nals {
        if let Some(au) = merger.merge_nal(nal) {
            out.push((au.packet, au.is_idr));
        }
    }
    out
}

fn decode_loop(
    input_rx: Receiver<VideoInput>,
    stop: Arc<AtomicBool>,
    frame_queue: Arc<LatestFrameQueue<DecodedFrame>>,
    stats: Arc<PipelineStatsAtomic>,
    decoder_preference: DecoderPreference,
    decoder_output_mode: DecoderOutputMode,
) {
    info!("decode thread started");
    let mut decoder = match DecoderFactory::create(decoder_preference, decoder_output_mode) {
        Ok(d) => d,
        Err(e) => {
            error!("decoder init failed: {}", e);
            return;
        }
    };
    info!("decode backend ready: name={}", decoder.name());
    let mut recoveries: u64 = 0;
    let mut au_count: u64 = 0;
    let mut raw_merger = H264AuMerger::new();
    let mut framed_merger = FramedPacketMerger::new();
    let mut sync_state = DecodeSyncState::NeedIdr;

    while !stop.load(Ordering::Acquire) {
        let input = match input_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let au = match input {
            VideoInput::RawNal(nal) => raw_merger.merge_nal(&nal),
            VideoInput::FramedPacket(pkt) => framed_merger.merge(pkt),
        };

        let Some(au) = au else {
            continue;
        };
        au_count = au_count.saturating_add(1);
        if au_count % 120 == 0 || au.is_idr || au.is_config {
            debug!(
                "decode input au: seq={}, bytes={}, is_idr={}, is_config={}, sync_state={:?}",
                au_count,
                au.packet.len(),
                au.is_idr,
                au.is_config,
                sync_state
            );
        }

        // 生产策略：失步后仅在 IDR 帧恢复解码，防止花屏持续扩散。
        if sync_state == DecodeSyncState::NeedIdr && !au.is_idr && !au.is_config {
            continue;
        }
        if sync_state == DecodeSyncState::NeedIdr && au.is_idr {
            info!("decoder resynced on IDR frame");
            sync_state = DecodeSyncState::Synced;
            // 上报“已完成重同步”信号，供运行时判断是否需要升级为重连。
            stats.resync_signals.fetch_add(1, Ordering::Relaxed);
        }

        // 记录单帧解码耗时，用于后续观测性能与回归。
        let start = Instant::now();
        match decoder.decode(&au.packet) {
            Ok(frames) => {
                if frames.is_empty() {
                    continue;
                }
                let decode_ms = start.elapsed().as_millis() as u64;
                stats.last_decode_ms.store(decode_ms, Ordering::Relaxed);
                debug!(
                    "decode output: seq={}, frames={}, cost_ms={}",
                    au_count,
                    frames.len(),
                    decode_ms
                );
                for frame in frames {
                    stats.decoded_frames.fetch_add(1, Ordering::Relaxed);
                    if frame_queue.push_latest(frame) {
                        stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        debug!("frame queue full, drop old frame");
                    }
                }
            }
            Err(e) => {
                error!("decode failed: {}", e);
                // 一旦发生解码错误，立即退回 NeedIdr，等待关键帧重同步。
                sync_state = DecodeSyncState::NeedIdr;
                stats.need_idr_signals.fetch_add(1, Ordering::Relaxed);
                // 在旋转/分辨率突变场景，硬解上下文可能失效。
                // 这里先做“原地重建解码器”恢复。
                if should_recreate_decoder(&e) {
                    warn!("recreate decoder...");
                    match DecoderFactory::create(decoder_preference, decoder_output_mode) {
                        Ok(new_decoder) => {
                            decoder = new_decoder;
                            recoveries += 1;
                            info!("decoder recreated, name={}, count={}", decoder.name(), recoveries);
                        }
                        Err(reinit_err) => {
                            error!("decoder recreate failed: {}", reinit_err);
                        }
                    }
                }
            }
        }
    }
    info!("decode thread exit");
}

fn should_recreate_decoder(err: &ScrcpyError) -> bool {
    // 基于错误文本做启发式判定，覆盖当前已观测到的
    // 硬解失效关键字（参数不兼容、句柄失效、外部库错误等）。
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("invalid argument")
        || msg.contains("avhwframescontext")
        || msg.contains("invalid handle")
        || msg.contains("external library")
        || msg.contains("resource temporarily unavailable")
}

fn upload_loop<F>(
    on_frame: &mut F,
    stop: Arc<AtomicBool>,
    frame_queue: Arc<LatestFrameQueue<DecodedFrame>>,
    stats: Arc<PipelineStatsAtomic>,
) where
    F: FnMut(DecodedFrame) -> Result<()>,
{
    let mut upload_count: u64 = 0;
    while !stop.load(Ordering::Acquire) {
        let Some(frame) = frame_queue.pop_wait(Duration::from_millis(10)) else {
            continue;
        };
        upload_count = upload_count.saturating_add(1);
        let frame_brief = match &frame {
            DecodedFrame::CpuBgra(f) => format!("cpu:{}x{} pts={}", f.width, f.height, f.pts),
            DecodedFrame::GpuShared {
                handle,
                width,
                height,
                pts,
            } => format!("gpu:handle={} {}x{} pts={}", handle, width, height, pts),
        };

        // 记录上传耗时，便于定位渲染链路瓶颈。
        let start = Instant::now();
        match on_frame(frame) {
            Ok(()) => {
                let upload_ms = start.elapsed().as_millis() as u64;
                stats.last_upload_ms.store(upload_ms, Ordering::Relaxed);
                stats.uploaded_frames.fetch_add(1, Ordering::Relaxed);
                if upload_count % 120 == 0 {
                    debug!(
                        "upload output: seq={}, frame={}, cost_ms={}",
                        upload_count, frame_brief, upload_ms
                    );
                }
            }
            Err(e) => {
                error!("upload frame failed: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::H264AuMerger;

    #[test]
    fn packet_merger_caches_config_and_emits_on_media() {
        let mut merger = H264AuMerger::new();

        let sps = vec![0x67, 0x64, 0x00, 0x1F];
        let pps = vec![0x68, 0xEE, 0x3C, 0x80];
        // first_mb_in_slice=0 的最小 IDR slice（Exp-Golomb code: "1"）
        let idr_1 = vec![0x65, 0x80, 0x00];
        let idr_2 = vec![0x65, 0x80, 0x00];

        assert!(merger.merge_nal(&sps).is_none());
        assert!(merger.merge_nal(&pps).is_none());
        assert!(merger.merge_nal(&idr_1).is_none());
        let out = merger
            .merge_nal(&idr_2)
            .expect("should emit previous access unit");

        // packet = [sc+sps][sc+pps][sc+idr_1]
        let mut expected = Vec::new();
        expected.extend_from_slice(&[0, 0, 0, 1]);
        expected.extend_from_slice(&sps);
        expected.extend_from_slice(&[0, 0, 0, 1]);
        expected.extend_from_slice(&pps);
        expected.extend_from_slice(&[0, 0, 0, 1]);
        expected.extend_from_slice(&idr_1);

        assert_eq!(out.packet, expected);
        assert!(out.is_idr);
    }
}
