#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_assignments)]
#![allow(unused_mut)]
#![allow(unused_must_use)]
// 完整流水线测试 - 按照main.rs的方式
#[path = "../src/adb/mod.rs"]
mod adb;
#[path = "../src/decoder/mod.rs"]
mod decoder;
#[path = "../src/config/mod.rs"]
mod config;
#[path = "../src/error.rs"]
mod error;
#[path = "../src/scrcpy/mod.rs"]
mod scrcpy;
#[path = "../tests/support/geometry.rs"]
mod geometry;
#[path = "../src/session/mod.rs"]
mod session;
#[path = "../src/utils/mod.rs"]
mod utils;
#[path = "../tests/support/window_helper.rs"]
mod window_helper;
#[path = "../tests/support/renderer.rs"]
mod renderer;

use adb::AdbClient;
use decoder::{
    D3D11Context, DecodedFrame, DecoderPipeline, DecoderPreference, PipelineConfig, PipelineEvent,
};
use error::Result;
use renderer::D3D11Renderer;
use scrcpy::control::{AndroidMotionEventAction, ControlChannel, TouchEvent};
use scrcpy::FramedVideoStreamReader;
use geometry::{map_window_touch, Orientation, PointI32, SizeU32};
use session::encoding_profile::EncodingProfile;
use session::manager::SessionManager;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;
use tracing::{info, warn, Level};
use windows::Win32::Foundation::HWND;

/// 测试链路解码策略常量：
/// - 默认“优先硬解，失败自动回退软解”，与生产策略一致；
/// - 如需强制软解回归，可改为 `DecoderPreference::ForceSoftware`。
const TEST_DECODER_PREFERENCE: DecoderPreference = DecoderPreference::PreferHardware;
/// 日志级别策略：
/// - 生产联调默认 `INFO`，避免逐帧日志淹没关键信息；
/// - 如需深度排查再临时改为 `DEBUG`。
const TEST_LOG_LEVEL: Level = Level::INFO;

/// 测试专用解码链路。
///
/// 与生产链路解耦：仅在 `test_full_pipeline` 中使用，
/// 删除该示例文件不会影响主工程编译。
struct TestDecodeChain {
    pipeline: DecoderPipeline,
    decoded_rx: mpsc::Receiver<DecodedFrame>,
}

impl TestDecodeChain {
    fn new() -> Result<Self> {
        let (decoded_tx, decoded_rx) = mpsc::channel::<DecodedFrame>();
        let pipeline_cfg = PipelineConfig {
            decoder_preference: TEST_DECODER_PREFERENCE,
            ..PipelineConfig::default()
        };
        let pipeline = DecoderPipeline::start(pipeline_cfg, move |frame| {
            decoded_tx.send(frame).map_err(|e| {
                error::ScrcpyError::Other(format!("向主线程回传解码帧失败: {}", e))
            })
        })?;
        Ok(Self { pipeline, decoded_rx })
    }

    fn push_framed_packet(
        &self,
        data: Vec<u8>,
        is_config: bool,
        is_keyframe: bool,
    ) -> Result<()> {
        self.pipeline
            .push_framed_packet(data, is_config, is_keyframe)
    }

    fn drain_frames(&self) -> Vec<DecodedFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = self.decoded_rx.try_recv() {
            out.push(frame);
        }
        out
    }

    fn stats(&self) -> decoder::PipelineStats {
        self.pipeline.stats()
    }
}

/// 测试专用 D3D 窗口显示链路。
///
/// 仅封装“窗口 + 上传 + 渲染”，不修改生产 Flutter 共享纹理代码。
struct TestD3dDisplay {
    hwnd: HWND,
    renderer: D3D11Renderer,
    video_size: (u32, u32),
}

impl TestD3dDisplay {
    fn new(shared_ctx: &D3D11Context, video_w: u32, video_h: u32) -> Result<Self> {
        let hwnd = unsafe {
            window_helper::create_test_window("D3D11 Test Window", video_w, video_h)
                .map_err(error::ScrcpyError::Other)?
        };
        let mut renderer = D3D11Renderer::new_with_context(hwnd.0, video_w, video_h, shared_ctx)
            .map_err(|e| error::ScrcpyError::Other(e.to_string()))?;
        renderer.set_video_size(video_w, video_h);
        Ok(Self {
            hwnd,
            renderer,
            video_size: (video_w, video_h),
        })
    }

    fn pump_and_resize(&mut self) -> bool {
        if !window_helper::pump_messages() {
            return false;
        }
        if let Some((cw, ch)) = window_helper::get_client_size(self.hwnd) {
            let _ = self.renderer.resize(cw as u32, ch as u32);
        }
        true
    }

    fn render_frame(&mut self, frame: &DecodedFrame) -> Result<()> {
        match frame {
            DecodedFrame::CpuBgra(frame) => {
                self.renderer
                    .render_bgra_frame(frame.width, frame.height, &frame.data)
                    .map_err(|e| error::ScrcpyError::Other(e.to_string()))
            }
            DecodedFrame::GpuShared {
                handle,
                width: _,
                height: _,
                pts: _,
            } => {
                warn!("gpu shared frame not connected in current test renderer, handle={}", handle);
                Ok(())
            }
        }
    }

    fn client_size(&self) -> (i32, i32) {
        window_helper::get_client_size(self.hwnd).unwrap_or((0, 0))
    }

    fn hwnd(&self) -> HWND {
        self.hwnd
    }

    fn video_size(&self) -> (u32, u32) {
        self.video_size
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 解码策略：
    // - prefer_hw（默认）：优先硬解，失败自动软解；
    // - force_hw：仅硬解，找不到硬解直接报错；
    // - force_sw：强制软解。
    // 协议策略：
    // - true：使用 scrcpy 分帧协议（raw_stream=false, send_frame_meta=true, send_codec_meta=true）；
    // - false：回退旧 raw NAL 流。
    // GPU 直出测试链路缓存配置（低延迟）：
    // - 小容量+消费即删除，避免“旧帧堆积”带来的延迟与内存上涨；
    // - 正式 Flutter 链路应由外部纹理生命周期管理替代这套测试缓存。
    // 解码/协议/共享缓存均使用代码常量配置，不依赖环境变量。
    // 调试开关：禁用 CUVID（仅在排查动态分辨率问题时使用）

    tracing_subscriber::fmt().with_max_level(TEST_LOG_LEVEL).init();
    // Ensure DPI-aware window sizing (avoids black bars on high-DPI displays)
    window_helper::init_dpi_awareness();

    info!("=== 完整视频流水线测试 ===");
    info!("流程: Android → scrcpy → FFmpeg → D3D11");

    // 1. 连接设备（先拿到视频尺寸）
    info!("[1/4] 连接Android设备...");
    let adb = AdbClient::new(PathBuf::from("D:/SoftwareEnv/scrcpy-server/adb.exe"));
    // let device_id = adb.get_first_device().await?;
    let device_ids = adb.list_devices().await?;
    let device_id = device_ids
        .first()
        .cloned()
        .ok_or_else(|| error::ScrcpyError::Other("未检测到可用 Android 设备".to_string()))?;

    let mut manager = SessionManager::new(
        adb,
        device_id,
        PathBuf::from("D:/SoftwareEnv/scrcpy-win64-v3.3.4/scrcpy-server"),
        27183,
        27184,
        EncodingProfile {
            max_size: 0,
            bit_rate: 8_000_000,
            max_fps: 0,
            intra_refresh_period: 0,
            video_encoder: None,
            turn_screen_off: true,
            stay_awake: false,
            force_landscape: false,
            scrcpy_log_level: "info".to_string(),
        },
        None,
    )?;

    let session = manager.connect_v2().await?;
    let mut server = session.server;
    let mut video_stream = session.video_stream;
    let mut control = ControlChannel::new(session.control_stream);
    info!("✅ 连接成功");
    // 设备物理分辨率（会话信息）。
    // 注意：触控消息中的 width/height 应与视频流分辨率一致，
    // 如果 max_size 降采样（例如 720）而仍使用物理分辨率，服务端会丢弃触控。
    let control_w = 1080u32;
    let control_h = 1920u32;
    // 触控注入应使用“视频流尺寸”，否则在降采样/裁剪时服务端会拒绝坐标。
    let stream_w = control_w;
    let stream_h = control_h;
    info!("📹 视频流元信息: 使用默认尺寸 {}x{}", stream_w, stream_h);

    // 2. D3D11上下文/上传器
    // 当前使用 framed 协议，不再依赖 raw NAL 中的 SPS 作为“窗口创建触发条件”，
    // 直接按会话分辨率创建窗口与渲染器，避免首帧前黑屏/无窗口。
    info!("[2/4] 初始化D3D11上下文...");
    let shared_ctx = D3D11Context::new().map_err(|e| error::ScrcpyError::Other(e.to_string()))?;
    let created_hwnd = unsafe {
        window_helper::create_test_window("D3D11 Test Window", stream_w, stream_h)
            .map_err(error::ScrcpyError::Other)?
    };
    let mut created_renderer =
        D3D11Renderer::new_with_context(created_hwnd.0, stream_w, stream_h, &shared_ctx)
            .map_err(|e| error::ScrcpyError::Other(e.to_string()))?;
    created_renderer.set_video_size(stream_w, stream_h);
    let mut renderer: Option<D3D11Renderer> = Some(created_renderer);
    let mut hwnd: Option<HWND> = Some(created_hwnd);
    let mut video_size: Option<(u32, u32)> = Some((stream_w, stream_h));
    if let Some((cw, ch)) = window_helper::get_client_size(created_hwnd) {
        info!(
            "🪟 初始窗口尺寸: client={}x{} (视频={}x{}, 手机={}x{})",
            cw, ch, stream_w, stream_h, control_w, control_h
        );
    }
    info!("✅ D3D11上下文/上传器创建成功");

    // 3. 解码流水线（生产链路：解码线程 + 帧消费线程）
    info!("[3/4] 初始化解码流水线...");
    let (decoded_tx, decoded_rx) = mpsc::channel::<DecodedFrame>();
    let (event_tx, event_rx) = mpsc::channel::<PipelineEvent>();
    let pipeline_cfg = PipelineConfig {
        // 回归测试也按 Flutter 线上策略：低延迟优先，避免队列堆积导致体感延迟升高。
        frame_queue_capacity: 2,
        nal_queue_capacity: 16,
        drop_nal_on_full: false,
        decoder_preference: TEST_DECODER_PREFERENCE,
        ..PipelineConfig::default()
    };
    let mut pipeline = DecoderPipeline::start_with_events(pipeline_cfg, move |frame| {
        decoded_tx
            .send(frame)
            .map_err(|e| error::ScrcpyError::Other(format!("向主线程回传解码帧失败: {}", e)))
    }, move |event| {
        event_tx.send(event).map_err(|e| {
            error::ScrcpyError::Other(format!("send pipeline event failed: {}", e))
        })
    })?;
    info!("✅ 解码流水线创建成功");
    // 4. 处理视频流 + 渲染
    info!("[4/4] 处理视频帧...");
    let mut reader = FramedVideoStreamReader::new(video_stream);
    let mut decoded_count: u64 = 0;
    let mut rendered_count: u64 = 0;
    let mut last_stat_ts = Instant::now();
    let mut packet_count_total = 0u64;
    let mut last_packet_count = 0u64;
    let mut last_decoded_count = 0u64;
    let mut last_rendered_count = 0u64;
    // 记录当前鼠标指针是否已经按下（用于保证 Up 事件可正常收尾）。
    let mut pointer_down = false;
    let mut render_paused = false;
    let mut render_pause_since: Option<Instant> = None;
    let mut paused_drop_frames: u64 = 0;
    let mut reconfigure_seq: u64 = 0;
    let mut current_reconfigure_seq: Option<u64> = None;
    let mut active_generation: u64 = 0;
    let mut last_rendered_handle: Option<i64> = None;
    let mut reconfigure_old_video_size: Option<(u32, u32)> = None;
    // 重配阶段保留“最后一帧”，避免恢复后因无新包而黑屏。
    let mut pending_frame_while_paused: Option<DecodedFrame> = None;
    info!("🔄 已进入固定方向测试模式（不执行运行时旋转切换）");

    loop {
        // info!("  等待读取帧 {}...", frame_count + 1);

        // 保持窗口消息泵不被长时间阻塞
        if renderer.is_some() && !window_helper::pump_messages() {
            info!("  收到退出消息");
            break;
        }

        if let (Some(hwnd), Some(ref mut renderer)) = (hwnd, renderer.as_mut()) {
            if let Some((cw, ch)) = window_helper::get_client_size(hwnd) {
                let _ = renderer.resize(cw as u32, ch as u32);
            }
        }

        // 消费解码内核事件：分辨率变化后更新渲染目标尺寸与触控映射基准。
        while let Ok(event) = event_rx.try_recv() {
            match event {
                PipelineEvent::ReconfigureBegin {
                    generation,
                    width,
                    height,
                } => {
                    active_generation = generation;
                    reconfigure_seq = reconfigure_seq.saturating_add(1);
                    current_reconfigure_seq = Some(reconfigure_seq);
                    reconfigure_old_video_size = video_size;
                    render_paused = true;
                    render_pause_since = Some(Instant::now());
                    paused_drop_frames = 0;
                    pending_frame_while_paused = None;
                    info!(
                        "RECONF_BEGIN seq={} gen={} target={}x{}，暂停渲染",
                        reconfigure_seq, generation, width, height
                    );
                    video_size = Some((width, height));
                    if let Some(ref mut renderer) = renderer {
                        // 关键：重配阶段清空旧共享句柄缓存，避免旋转后继续使用失效资源。
                        renderer.reset_shared_resources();
                        renderer.set_video_size(width, height);
                    }
                }
                PipelineEvent::ResolutionChanged {
                    generation,
                    width,
                    height,
                } => {
                    active_generation = generation;
                    if video_size != Some((width, height)) {
                        info!("流水线事件: gen={} 分辨率切换为 {}x{}", generation, width, height);
                        video_size = Some((width, height));
                        if let Some(ref mut renderer) = renderer {
                            renderer.set_video_size(width, height);
                        }
                    }
                }
                PipelineEvent::ReconfigureReady {
                    generation,
                    width,
                    height,
                } => {
                    active_generation = generation;
                    render_paused = false;
                    let paused_ms = render_pause_since
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    render_pause_since = None;
                    if paused_drop_frames > 0 {
                        info!(
                            "RECONF_DROP seq={:?} dropped_frames={}",
                            current_reconfigure_seq,
                            paused_drop_frames
                        );
                    }
                    info!(
                        "RECONF_READY seq={:?} gen={} size={}x{} pause_ms={}，恢复渲染",
                        current_reconfigure_seq, generation, width, height, paused_ms
                    );
                    if let Some(hwnd) = hwnd {
                        if let Some((old_w, old_h)) = reconfigure_old_video_size {
                            if old_w != width || old_h != height {
                                if let Some((before_w, before_h)) = window_helper::get_client_size(hwnd) {
                                    match window_helper::resize_window_for_content(
                                        hwnd,
                                        (old_w, old_h),
                                        (width, height),
                                    ) {
                                        Ok((after_w, after_h)) => info!(
                                            "WINDOW_AUTOFIT seq={:?} old_video={}x{} new_video={}x{} client:{}x{}->{}x{}",
                                            current_reconfigure_seq, old_w, old_h, width, height, before_w, before_h, after_w, after_h
                                        ),
                                        Err(e) => warn!(
                                            "WINDOW_AUTOFIT_FAIL seq={:?} old_video={}x{} new_video={}x{} err={}",
                                            current_reconfigure_seq, old_w, old_h, width, height, e
                                        ),
                                    }
                                }
                            }
                        }
                    }
                    current_reconfigure_seq = None;
                    reconfigure_old_video_size = None;
                    video_size = Some((width, height));
                    if let Some(ref mut renderer) = renderer {
                        renderer.set_video_size(width, height);
                    }
                }
            }
        }

        // 一旦恢复渲染，优先消费重配期间缓存的最后一帧，避免黑屏卡住。
        if !render_paused {
            if let Some(decoded_frame) = pending_frame_while_paused.take() {
                if let Some(ref mut renderer) = renderer {
                    match decoded_frame {
                        DecodedFrame::CpuBgra(bgra_frame) => {
                            if video_size != Some((bgra_frame.width, bgra_frame.height)) {
                                info!(
                                    "缓存帧兜底: 分辨率更新为 {}x{}",
                                    bgra_frame.width, bgra_frame.height
                                );
                                video_size = Some((bgra_frame.width, bgra_frame.height));
                                renderer.set_video_size(bgra_frame.width, bgra_frame.height);
                            }
                            renderer
                                .render_bgra_frame(
                                    bgra_frame.width,
                                    bgra_frame.height,
                                    &bgra_frame.data,
                                )
                                .map_err(|e| error::ScrcpyError::Other(e.to_string()))?;
                            rendered_count = rendered_count.saturating_add(1);
                        }
                        DecodedFrame::GpuShared {
                            handle,
                            width,
                            height,
                            pts: _,
                        } => {
                            if video_size != Some((width, height)) {
                                info!("缓存帧兜底: 分辨率更新为 {}x{}", width, height);
                                video_size = Some((width, height));
                                renderer.set_video_size(width, height);
                            }
                            renderer
                                .render_shared_handle(handle as u64)
                                .map_err(|e| error::ScrcpyError::Other(e.to_string()))?;
                            rendered_count = rendered_count.saturating_add(1);
                        }
                    }
                }
            }
        }

        // 将鼠标按住滑动传递给设备
        if let Some(hwnd) = hwnd {
            if render_paused {
                if let Some(since) = render_pause_since {
                    let paused_ms = since.elapsed().as_millis();
                    if paused_ms > 1500 {
                        warn!(
                            "渲染已暂停 {} ms，等待重配完成事件",
                            paused_ms
                        );
                        // 避免每帧刷屏，重置计时窗口。
                        render_pause_since = Some(Instant::now());
                    }
                }
            }
            for ev in window_helper::drain_mouse_events() {
                if render_paused {
                    // 重配阶段丢弃触控输入，避免旧坐标系污染。
                    continue;
                }
                if let Some((cw, ch)) = window_helper::get_client_size(hwnd) {
                    // 使用 scrcpy 风格映射：window -> drawable(HiDPI) -> content rect -> frame -> normalized。
                    // 当前测试窗口没有单独 drawable 尺寸接口，先按 1:1 处理（如后续有 HiDPI drawable 可替换）。
                    let frame = if let Some((vw, vh)) = video_size {
                        SizeU32 {
                            width: vw.max(1),
                            height: vh.max(1),
                        }
                    } else {
                        SizeU32 {
                            width: control_w.max(1),
                            height: control_h.max(1),
                        }
                    };

                    let mapped = map_window_touch(
                        PointI32 { x: ev.x, y: ev.y },
                        SizeU32 {
                            width: cw.max(1) as u32,
                            height: ch.max(1) as u32,
                        },
                        SizeU32 {
                            width: cw.max(1) as u32,
                            height: ch.max(1) as u32,
                        },
                        frame,
                        Orientation::Deg0,
                    );

                    let Some(mapped) = mapped else {
                        continue;
                    };

                    let (action, pressure, buttons) = match ev.kind {
                        window_helper::MouseEventKind::Down => {
                            if !mapped.inside_content {
                                continue;
                            }
                            pointer_down = true;
                            (AndroidMotionEventAction::Down, 1.0, 1)
                        }
                        window_helper::MouseEventKind::Up => {
                            if !pointer_down {
                                continue;
                            }
                            pointer_down = false;
                            (AndroidMotionEventAction::Up, 0.0, 0)
                        }
                        window_helper::MouseEventKind::Move => {
                            if !pointer_down {
                                continue;
                            }
                            (AndroidMotionEventAction::Move, 1.0, 1)
                        }
                    };

                    let _ = control
                        .send_touch_event(&TouchEvent {
                            action,
                            pointer_id: -1, // 鼠标模式
                            x: mapped.norm_x,
                            y: mapped.norm_y,
                            pressure,
                            // 使用当前视频分辨率作为注入基准，避免旋转/降采样后触控被服务端丢弃。
                            width: frame.width,
                            height: frame.height,
                            buttons,
                        })
                        .await;
                }
            }
        }

        // 消费解码输出（低延迟+防卡死策略）：
        // 1) 单轮最多消费固定数量，避免主线程被“清空队列”长期占用；
        // 2) 只渲染最新帧，旧帧直接丢弃，防止旋转/高帧率时 UI 被历史帧拖死。
        const MAX_DECODE_PULL_PER_TICK: usize = 32;
        let mut latest_frame: Option<DecodedFrame> = None;
        for _ in 0..MAX_DECODE_PULL_PER_TICK {
            match decoded_rx.try_recv() {
                Ok(frame) => {
                    decoded_count = decoded_count.saturating_add(1);
                    latest_frame = Some(frame);
                }
                Err(_) => break,
            }
        }

        if let Some(decoded_frame) = latest_frame {
            if let Some(ref mut renderer) = renderer {
                match decoded_frame {
                    DecodedFrame::CpuBgra(bgra_frame) => {
                        if render_paused {
                            paused_drop_frames = paused_drop_frames.saturating_add(1);
                            pending_frame_while_paused = Some(DecodedFrame::CpuBgra(bgra_frame));
                            if paused_drop_frames % 120 == 0 {
                                warn!(
                                    "渲染暂停中: 已丢弃解码帧={}, 最新帧={}x{} pts={}",
                                    paused_drop_frames,
                                    if let Some(DecodedFrame::CpuBgra(f)) =
                                        pending_frame_while_paused.as_ref()
                                    {
                                        f.width
                                    } else {
                                        0
                                    },
                                    if let Some(DecodedFrame::CpuBgra(f)) =
                                        pending_frame_while_paused.as_ref()
                                    {
                                        f.height
                                    } else {
                                        0
                                    },
                                    if let Some(DecodedFrame::CpuBgra(f)) =
                                        pending_frame_while_paused.as_ref()
                                    {
                                        f.pts
                                    } else {
                                        0
                                    }
                                );
                            }
                        } else {
                            if video_size != Some((bgra_frame.width, bgra_frame.height)) {
                                info!(
                                    "帧兜底: 分辨率更新为 {}x{}",
                                    bgra_frame.width, bgra_frame.height
                                );
                                video_size = Some((bgra_frame.width, bgra_frame.height));
                                renderer.set_video_size(bgra_frame.width, bgra_frame.height);
                            }
                            renderer
                                .render_bgra_frame(
                                    bgra_frame.width,
                                    bgra_frame.height,
                                    &bgra_frame.data,
                                )
                                .map_err(|e| error::ScrcpyError::Other(e.to_string()))?;
                            rendered_count = rendered_count.saturating_add(1);
                        }
                    }
                    DecodedFrame::GpuShared {
                        handle,
                        width,
                        height,
                        pts,
                    } => {
                        if render_paused {
                            paused_drop_frames = paused_drop_frames.saturating_add(1);
                            pending_frame_while_paused = Some(DecodedFrame::GpuShared {
                                handle,
                                width,
                                height,
                                pts,
                            });
                        } else {
                            if video_size != Some((width, height)) {
                                info!("GPU 帧兜底: 分辨率更新为 {}x{}", width, height);
                                video_size = Some((width, height));
                                renderer.set_video_size(width, height);
                            }
                            renderer
                                .render_shared_handle(handle as u64)
                                .map_err(|e| error::ScrcpyError::Other(e.to_string()))?;
                            rendered_count = rendered_count.saturating_add(1);
                            if last_rendered_handle != Some(handle) {
                                info!(
                                    "HANDLE_SWITCH old={:?} new={} size={}x{}",
                                    last_rendered_handle, handle, width, height
                                );
                                last_rendered_handle = Some(handle);
                            }
                            if rendered_count % 120 == 0 {
                                info!(
                                    "GPU 共享帧已渲染: handle={} size={}x{} pts={}",
                                    handle, width, height, pts
                                );
                            }
                        }
                    }
                }
            }
        }

        // 每秒打印一次链路统计，便于回归定位：
        // 1) 手机物理分辨率（Device Profile）；
        // 2) 视频流分辨率（scrcpy framed codec meta）；
        // 3) 当前窗口 client 分辨率（实际显示区域）；
        // 4) 包输入、解码、上传、渲染速率。
        if last_stat_ts.elapsed().as_secs_f32() >= 1.0 {
            let packet_count = packet_count_total;
            let packet_delta = packet_count.saturating_sub(last_packet_count);
            let decoded_delta = decoded_count.saturating_sub(last_decoded_count);
            let render_delta = rendered_count.saturating_sub(last_rendered_count);
            let stats = pipeline.stats();
            let window_client = hwnd
                .and_then(window_helper::get_client_size)
                .unwrap_or((0, 0));
            info!(
                "链路统计: pkt_total={} pkt_delta={} decoded_total={} decoded_delta={} rendered_total={} render_delta={} decoded={} uploaded={} dropped_frames={} dropped_nals={} decode_ms={} upload_ms={} window={}x{}",
                packet_count,
                packet_delta,
                decoded_count,
                decoded_delta,
                rendered_count,
                render_delta,
                stats.decoded_frames,
                stats.uploaded_frames,
                stats.dropped_frames,
                stats.dropped_nals,
                stats.last_decode_ms,
                stats.last_upload_ms,
                window_client.0,
                window_client.1
            );

            last_packet_count = packet_count;
            last_decoded_count = decoded_count;
            last_rendered_count = rendered_count;
            last_stat_ts = Instant::now();
        }

        // 使用短超时，避免阻塞导致窗口无响应
        let frame_result = tokio::time::timeout(
            tokio::time::Duration::from_millis(33),
            reader.read_packet(),
        )
        .await;

        match frame_result {
            Ok(Ok(Some(packet))) => {
                packet_count_total = packet_count_total.saturating_add(1);
                if packet.is_config || packet.is_keyframe {
                    info!(
                        "收到编码包: config={} key={} size={}",
                        packet.is_config,
                        packet.is_keyframe,
                        packet.data.len()
                    );
                }
                // 分帧协议输入：
                // - is_config：配置包（SPS/PPS 等），交由 pipeline 的 framed merger 缓存；
                // - is_keyframe：关键帧标记，用于解码失步后的快速重同步；
                // - data：一个完整编码包，不再走客户端 NAL/AU 猜测。
                pipeline.push_framed_packet(
                    packet.data.to_vec(),
                    packet.is_config,
                    packet.is_keyframe,
                )?;
            }
            Ok(Ok(None)) => {
                info!("  视频流结束");
                break;
            }
            Ok(Err(e)) => {
                info!("  读取帧错误: {}", e);
                break;
            }
            Err(_) => {
                // 超时就继续循环，保持 UI 响应
                continue;
            }
        }
    }

    info!(
        "🎉 测试完成! 解码帧={} 渲染帧={}",
        decoded_count, rendered_count
    );
    server.stop().await?;
    Ok(())
}
