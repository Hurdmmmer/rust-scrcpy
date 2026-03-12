//! Scrcpy 新架构服务入口（供 Flutter API 内部调用）。
//!
//! 关键约束：
//! - 不改变 `gh_api/flutter_api.rs` 的对外函数签名；
//! - 不改变 `flutter_callback_register` 的 C ABI 注册接口；
//! - 全部内部逻辑走 `scrcpy/*` 新目录。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use once_cell::sync::Lazy;
use tokio::process::Command;
use tracing::{Event, Level, Subscriber, info, warn};
use tracing::field::{Field, Visit};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, layer::SubscriberExt, registry::LookupSpan};
use tracing_subscriber::filter::LevelFilter;


use crate::flutter_callback_register;
use crate::gh_common::model::{
    DecoderMode, DeviceInfo, KeyEvent, LogLevel, OrientationChangeSource, OrientationMode,
    RenderPipelineMode, ScrollEvent, SessionConfig, SessionConfigV2, SessionEvent, SessionStats,
    SystemKey, TouchEvent,
};
use crate::gh_common::{Result, ScrcpyError};
use crate::scrcpy::client::scrcpy_control::{
    AndroidKeyEventAction, AndroidMotionEventAction, KeyEvent as AndroidKeyEvent,
    ScrollEvent as AndroidScrollEvent, TouchEvent as AndroidTouchEvent,
};
use crate::scrcpy::client::ScrcpyClient;
use crate::scrcpy::config::ScrcpyClientConfig;
use crate::scrcpy::input::ScrcpyInputMode;
use crate::scrcpy::runtime::{ScrcpyCoreRuntime, ScrcpyDecodeConfig};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 会话 ID 递增计数器。
///
/// 设计目的：
/// - 对外暴露的会话 ID 只要求在进程内唯一，不要求跨进程稳定；
/// - 使用原子递增可以避免全局锁竞争，并且实现简单可预测。
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// 全局会话表。
///
/// 表中保存 API 层会话状态，键为 `session_id`。
/// 这里不做持久化，进程退出后会话自然失效。
static API_SESSIONS: Lazy<Mutex<HashMap<String, ApiSession>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 日志初始化标记。
///
/// tracing subscriber 在同一进程里只能初始化一次，
/// 这里用布尔位保证 `setup_logger` 幂等。
static LOGGER_READY: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

/// tracing 字段访问器：将事件字段拼接成文本，便于回传到 Flutter 日志面板。
struct FlutterLogVisitor {
    fields: Vec<String>,
}

impl FlutterLogVisitor {
    /// 输出字段文本（`key=value`，逗号分隔）。
    fn as_text(&self) -> String {
        self.fields.join(", ")
    }
}

impl Visit for FlutterLogVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.push(format!("{}={:?}", field.name(), value));
    }
}

/// tracing -> Flutter 日志桥接层。
///
/// 作用：
/// - 监听每条 tracing event；
/// - 抽取 level/target/fields；
/// - 通过 `flutter_callback_register::notify_rust_log` 推送到 Runner。
struct FlutterLogLayer;

impl<S> Layer<S> for FlutterLogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = FlutterLogVisitor { fields: Vec::new() };
        event.record(&mut visitor);
        let fields_text = visitor.as_text();
        let message = if fields_text.is_empty() {
            format!("target={}", meta.target())
        } else {
            format!("target={} {}", meta.target(), fields_text)
        };
        crate::flutter_callback_register::notify_rust_log(
            &meta.level().to_string(),
            &message,
        );
    }
}

/// 后台 worker 控制命令。
///
/// 架构说明：
/// - API 线程不直接触碰运行时对象；
/// - 所有控制行为都转成命令，通过通道投递给会话 worker；
/// - 这样可以把并发访问收敛到单线程，减少锁和状态竞争。
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

/// 单会话后台 worker。
///
/// 字段语义：
/// - `tx`：API -> worker 命令通道；
/// - `running`：跨线程可见的运行标记，用于快速退出；
/// - `stats`：最近一次统计快照，供 API 线程读取；
/// - `join`：线程句柄，用于 stop/dispose 阶段阻塞回收。
struct RuntimeWorker {
    tx: Sender<RuntimeCommand>,
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<SessionStats>>,
    join: Option<JoinHandle<()>>,
}

/// API 层会话状态。
///
/// 该结构只保存“配置与运行句柄”，不存放重型 runtime 对象，
/// 避免在全局会话表中持有不可 Send 的复杂状态。
struct ApiSession {
    config: SessionConfig,
    decoder_mode: DecoderMode,
    render_pipeline_mode: RenderPipelineMode,
    worker: Option<RuntimeWorker>,
}

/// 获取全局会话表互斥锁。
///
/// 统一在这里转换 poison 错误，避免每个调用点重复错误映射代码。
fn lock_sessions() -> Result<MutexGuard<'static, HashMap<String, ApiSession>>> {
    API_SESSIONS
        .lock()
        .map_err(|_| ScrcpyError::Other("api session map poisoned".to_string()))
}

/// 生成新会话 ID。
///
/// 返回格式固定为 `sess-{number}`，便于日志检索与排查。
fn new_session_id() -> String {
    format!("sess-{}", NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed))
}

/// 统一构造“会话不存在”错误。
///
/// 这样可以保证不同 API 的错误文案一致，减少前端判断分支。
fn invalid_session_error(session_id: &str) -> ScrcpyError {
    ScrcpyError::Other(format!("invalid session id: {}", session_id))
}

/// 将业务日志级别映射为 tracing 级别。
///
/// 该映射是 1:1 语义映射，不做降级/升级转换。
fn map_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Trace => Level::TRACE,
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Info => Level::INFO,
        LogLevel::Warn => Level::WARN,
        LogLevel::Error => Level::ERROR,
    }
}

/// 创建默认统计对象。
///
/// 在会话尚未启动或统计尚不可用时返回，避免 API 侧出现空值判断。
fn default_stats() -> SessionStats {
    SessionStats {
        fps: 0.0,
        decode_latency_ms: 0,
        upload_latency_ms: 0,
        total_frames: 0,
        dropped_frames: 0,
    }
}

/// 将 Flutter API 会话配置映射为新架构客户端配置。
///
/// 约束：
/// - 字段语义尽量保持直通，避免隐式换算；
/// - 仅在字段名不同场景下做机械映射，避免引入配置漂移。
fn map_client_config(config: &SessionConfig) -> ScrcpyClientConfig {
    ScrcpyClientConfig {
        adb_path: config.adb_path.clone(),
        server_path: config.server_path.clone(),
        device_id: config.device_id.clone(),
        video_port: config.video_port,
        control_port: config.control_port,
        max_size: config.max_size,
        bit_rate: config.bit_rate,
        max_fps: config.max_fps,
        intra_refresh_period: config.intra_refresh_period,
        video_encoder: config.video_encoder.clone(),
        turn_screen_off: config.turn_screen_off,
        stay_awake: config.stay_awake,
        scrcpy_log_level: config.scrcpy_verbosity.clone(),
        // 按产品要求强制使用 UHID 键盘输入。
        input_mode: ScrcpyInputMode::Uhid,
    }
}

/// 将 API 层解码/渲染模式映射为新架构 decode pipeline 配置。
///
/// 当前策略：
/// - `Original` 走 GPU 共享句柄链路；
/// - `CpuPixelBufferV2` 走 CPU BGRA 链路。
fn map_decode_config(
    decoder_mode: DecoderMode,
    render_pipeline_mode: RenderPipelineMode,
) -> ScrcpyDecodeConfig {
    let mut cfg = ScrcpyDecodeConfig::default();
    cfg.decoder_mode = decoder_mode;
    cfg.output_mode = match render_pipeline_mode {
        RenderPipelineMode::Original => crate::scrcpy::decode_core::DecoderOutputMode::GpuShared,
        RenderPipelineMode::CpuPixelBufferV2 => crate::scrcpy::decode_core::DecoderOutputMode::CpuBgra,
    };
    cfg
}

/// 将 Flutter API 侧触摸事件映射为新 runtime 控制事件。
///
/// 这里按协议 action 数值做映射，不直接依赖历史目录枚举类型。
/// 当 action 非法时，默认降级为 `Move`，避免输入异常导致线程崩溃。
fn map_touch_event(event: TouchEvent) -> AndroidTouchEvent {
    let action = match event.action as u8 {
        0 => AndroidMotionEventAction::Down,
        1 => AndroidMotionEventAction::Up,
        2 => AndroidMotionEventAction::Move,
        3 => AndroidMotionEventAction::Cancel,
        5 => AndroidMotionEventAction::PointerDown,
        6 => AndroidMotionEventAction::PointerUp,
        7 => AndroidMotionEventAction::HoverMove,
        9 => AndroidMotionEventAction::HoverEnter,
        10 => AndroidMotionEventAction::HoverExit,
        _ => AndroidMotionEventAction::Move,
    };
    AndroidTouchEvent {
        action,
        pointer_id: event.pointer_id,
        x: event.x,
        y: event.y,
        pressure: event.pressure,
        width: event.width,
        height: event.height,
        buttons: event.buttons,
    }
}

/// 将 Flutter API 侧按键事件映射为新 runtime 控制事件。
///
/// 当 action 非法时，默认降级为 `Down`，保证控制通道消息仍可发送。
fn map_key_event(event: KeyEvent) -> AndroidKeyEvent {
    let action = match event.action as u8 {
        0 => AndroidKeyEventAction::Down,
        1 => AndroidKeyEventAction::Up,
        _ => AndroidKeyEventAction::Down,
    };
    AndroidKeyEvent {
        action,
        keycode: event.keycode,
        repeat: event.repeat,
        metastate: event.metastate,
    }
}

/// 将 Flutter API 侧滚轮事件映射为新 runtime 控制事件。
///
/// 该函数不做坐标系换算，只做结构搬运，换算由控制层处理。
fn map_scroll_event(event: ScrollEvent) -> AndroidScrollEvent {
    AndroidScrollEvent {
        x: event.x,
        y: event.y,
        width: event.width,
        height: event.height,
        hscroll: event.hscroll,
        vscroll: event.vscroll,
    }
}

/// 将语义系统键映射为 Android keycode。
///
/// `RotateScreen` 暂无直接 keycode 对应，返回 `None`。
fn system_key_to_keycode(key: SystemKey) -> Option<u32> {
    match key {
        SystemKey::Home => Some(3),
        SystemKey::Back => Some(4),
        SystemKey::Recent => Some(187),
        SystemKey::PowerMenu => Some(26),
        SystemKey::VolumeUp => Some(24),
        SystemKey::VolumeDown => Some(25),
        SystemKey::RotateScreen => None,
    }
}

/// 把会话事件序列化为 JSON，并投递到 Flutter 回调注册器。
///
/// 失败策略：
/// - 序列化失败时静默忽略（不 panic、不阻断主流程）；
/// - 原因是事件通知属于旁路能力，不应拖垮主会话链路。
fn notify_session_event_json(session_id: &str, event: &SessionEvent) {
    if let Ok(payload) = serde_json::to_vec(event) {
        flutter_callback_register::notify_session_event(session_id, &payload);
    }
}

/// 直接执行 adb 命令并返回 stdout 文本。
///
/// 行为约束：
/// - stdout/stderr 都捕获，便于失败诊断；
/// - 非零退出码统一映射为 `ScrcpyError::Other`；
/// - Windows 下关闭弹窗，避免影响桌面应用体验。
async fn adb_execute(adb_path: &str, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(adb_path);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd
        .output()
        .await
        .map_err(|e| ScrcpyError::Other(format!("adb execute failed: {}", e)))?;

    if !output.status.success() {
        return Err(ScrcpyError::Other(format!(
            "adb command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 执行设备方向模式切换。
///
/// 该能力必须走 adb 系统设置，而不是仅发送事件，
/// 否则会出现“上层看到方向变化事件，但设备方向没有变化”的假状态。
async fn adb_set_orientation_mode(
    adb_path: &str,
    device_id: &str,
    mode: OrientationMode,
) -> Result<()> {
    // Android 11+/12+/13 上，`cmd window user-rotation` 通常比直接写 settings 更可靠。
    // 这里采用“新命令优先，旧命令兜底”的策略，避免部分 ROM 上方向设置无效。
    async fn adb_shell_try(adb_path: &str, device_id: &str, command: &str) -> Result<()> {
        let _ = adb_execute(adb_path, &["-s", device_id, "shell", command]).await?;
        Ok(())
    }

    match mode {
        OrientationMode::Auto => {
            if adb_shell_try(adb_path, device_id, "cmd window user-rotation free")
                .await
                .is_err()
            {
                let _ = adb_execute(
                    adb_path,
                    &["-s", device_id, "shell", "settings put system accelerometer_rotation 1"],
                )
                .await?;
            }
        }
        OrientationMode::Portrait => {
            if adb_shell_try(adb_path, device_id, "cmd window user-rotation lock 0")
                .await
                .is_err()
            {
                let _ = adb_execute(
                    adb_path,
                    &["-s", device_id, "shell", "settings put system accelerometer_rotation 0"],
                )
                .await?;
                let _ = adb_execute(
                    adb_path,
                    &["-s", device_id, "shell", "settings put system user_rotation 0"],
                )
                .await?;
            }
        }
        OrientationMode::Landscape => {
            if adb_shell_try(adb_path, device_id, "cmd window user-rotation lock 1")
                .await
                .is_err()
            {
                let _ = adb_execute(
                    adb_path,
                    &["-s", device_id, "shell", "settings put system accelerometer_rotation 0"],
                )
                .await?;
                let _ = adb_execute(
                    adb_path,
                    &["-s", device_id, "shell", "settings put system user_rotation 1"],
                )
                .await?;
            }
        }
    }

    Ok(())
}
/// 处理一条运行时命令。
///
/// 返回值语义：
/// - `true`：继续运行；
/// - `false`：收到停止信号，退出主循环。
async fn handle_runtime_command(
    runtime: &mut ScrcpyCoreRuntime,
    session_id: &str,
    config: &SessionConfig,
    cmd: RuntimeCommand,
) -> bool {
    match cmd {
        RuntimeCommand::Stop => false,
        RuntimeCommand::Touch(e) => {
            let _ = runtime.send_touch(session_id, &map_touch_event(e)).await;
            true
        }
        RuntimeCommand::Key(e) => {
            let _ = runtime.send_key(session_id, &map_key_event(e)).await;
            true
        }
        RuntimeCommand::Scroll(e) => {
            let _ = runtime.send_scroll(session_id, &map_scroll_event(e)).await;
            true
        }
        RuntimeCommand::Text(text) => {
            let _ = runtime.send_text(session_id, &text).await;
            true
        }
        RuntimeCommand::Clipboard { text, paste } => {
            let _ = runtime.set_clipboard(session_id, &text, paste).await;
            true
        }
        RuntimeCommand::SystemKey(key) => {
            if let Some(keycode) = system_key_to_keycode(key) {
                let down = AndroidKeyEvent {
                    action: AndroidKeyEventAction::Down,
                    keycode,
                    repeat: 0,
                    metastate: 0,
                };
                let up = AndroidKeyEvent {
                    action: AndroidKeyEventAction::Up,
                    keycode,
                    repeat: 0,
                    metastate: 0,
                };
                let _ = runtime.send_key(session_id, &down).await;
                let _ = runtime.send_key(session_id, &up).await;
            }
            true
        }
        RuntimeCommand::SetOrientation(mode) => {
            match adb_set_orientation_mode(&config.adb_path, &config.device_id, mode).await {
                Ok(_) => {
                    notify_session_event_json(
                        session_id,
                        &SessionEvent::OrientationChanged {
                            mode,
                            source: OrientationChangeSource::ManualApi,
                        },
                    );
                }
                Err(e) => {
                    notify_session_event_json(
                        session_id,
                        &SessionEvent::Error {
                            code: crate::gh_common::model::ErrorCode::ControlFailed,
                            message: format!("set orientation failed: {}", e),
                        },
                    );
                }
            }
            true
        }
        RuntimeCommand::RequestIdr => {
            let _ = runtime.request_idr(session_id).await;
            true
        }
    }
}
/// 启动单会话后台线程。
///
/// 线程职责：
/// - 启动并持有 runtime；
/// - 消费控制命令；
/// - 持续解码泵送；
/// - 检测重连信号并执行自动重连闭环。
fn spawn_runtime_worker(
    session_id: String,
    config: SessionConfig,
    decoder_mode: DecoderMode,
    render_pipeline_mode: RenderPipelineMode,
) -> RuntimeWorker {
    // 使用有界异步通道，构建 Selector 风格事件循环。
    // 目的：命令与视频解码竞争执行权，Stop 命令可快速抢占。
    let (tx, mut rx): (Sender<RuntimeCommand>, Receiver<RuntimeCommand>) = mpsc::channel(256);
    let running = Arc::new(AtomicBool::new(true));
    let stats = Arc::new(Mutex::new(default_stats()));

    let running_c = Arc::clone(&running);
    let stats_c = Arc::clone(&stats);

    let session_id_c = session_id.clone();

    // 每个会话独占一个 worker 线程，避免跨会话状态串扰。
    let join = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(v) => v,
            Err(e) => {
                warn!("[新服务] 创建 tokio runtime 失败: {}", e);
                notify_session_event_json(
                    &session_id_c,
                    &SessionEvent::Error {
                        code: crate::gh_common::model::ErrorCode::Internal,
                        message: format!("runtime init failed: {}", e),
                    },
                );
                running_c.store(false, Ordering::Relaxed);
                return;
            }
        };

        rt.block_on(async move {
            let client = ScrcpyClient::new(map_client_config(&config));
            let mut runtime = ScrcpyCoreRuntime::new(client);
            runtime.set_decode_config(map_decode_config(decoder_mode, render_pipeline_mode));

            // 会话启动前执行设备防休眠策略。
            if config.stay_awake {
                if let Err(e) = adb_execute(
                    &config.adb_path,
                    &["-s", &config.device_id, "shell", "svc power stayon true"],
                )
                .await
                {
                    warn!(
                        "[新服务] 设置设备防休眠失败（继续启动）: session_id={}, err={}",
                        session_id_c, e
                    );
                }
            }

            if let Err(e) = runtime.start(session_id_c.clone()).await {
                warn!("[新服务] 启动会话失败: session_id={}, err={}", session_id_c, e);
                running_c.store(false, Ordering::Relaxed);
                return;
            }

            // 会话启动后按配置执行一次熄屏策略。
            if config.turn_screen_off {
                if let Err(e) = runtime.set_display_power(&session_id_c, false).await {
                    warn!(
                        "[新服务] 会话启动后熄屏失败（不中断会话）: session_id={}, err={}",
                        session_id_c, e
                    );
                }
            }

            // Selector 风格主循环：
            // - `rx.recv()` 代表控制面事件；
            // - `decode_pump_once` 代表数据面事件；
            // - 使用 `biased` 让控制面优先，确保 Stop 等命令更快生效；
            // - 每轮只做一次解码泵送，避免单轮 budget 过大导致控制面饥饿。
            while running_c.load(Ordering::Relaxed) {
                tokio::select! {
                    biased;
                    cmd = rx.recv() => {
                        match cmd {
                            // 处理每次收到的控制命令。键盘输入，剪切板操作等。
                            Some(cmd) => {
                                let keep_running = handle_runtime_command(
                                    &mut runtime,
                                    &session_id_c,
                                    &config,
                                    cmd,
                                ).await;
                                if !keep_running {
                                    running_c.store(false, Ordering::Relaxed);
                                }
                            }
                            None => {
                                // 命令发送端已关闭，结束 worker。
                                running_c.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                    // 开始读取Scrcpy Server 发送的视频帧。
                    pump_result = runtime.decode_pump_once(&session_id_c) => {
                        if let Err(e) = pump_result {
                            warn!(
                                "[新服务] 解码泵送失败，结束会话: session_id={}, err={}",
                                session_id_c, e
                            );
                            running_c.store(false, Ordering::Relaxed);
                        }
                    }
                }

                if !running_c.load(Ordering::Relaxed) {
                    break;
                }

                // 自动重连闭环：stop -> 短等待 -> start。
                if let Ok(true) = runtime.decode_reconnect_required(&session_id_c) {
                    warn!("[新服务] 检测到重连信号，执行自动重连: session_id={}", session_id_c);
                    let _ = runtime.stop(&session_id_c).await;
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    if let Err(e) = runtime.start(session_id_c.clone()).await {
                        warn!(
                            "[新服务] 自动重连失败，结束会话: session_id={}, err={}",
                            session_id_c, e
                        );
                        running_c.store(false, Ordering::Relaxed);
                        break;
                    }
                }

                // 统计快照按循环覆盖写入，API 读取到的是最近一次可用值。
                if let Ok(s) = runtime.session_stats(&session_id_c) {
                    if let Ok(mut g) = stats_c.lock() {
                        *g = s;
                    }
                }
            }

            // Worker 线程收到退出信号后，会在退出前兜底停止 runtime，确保连接与资源被释放。
            let _ = runtime.stop(&session_id_c).await;
            running_c.store(false, Ordering::Relaxed);
        });
    });

    RuntimeWorker {
        tx,
        running,
        stats,
        join: Some(join),
    }
}

/// 初始化 Rust 侧日志系统。
///
/// 幂等语义：
/// - 同一进程只允许初始化一次；
/// - 后续重复调用直接返回成功，不会重复安装 subscriber。
pub async fn setup_logger(max_level: LogLevel) -> Result<()> {
    let mut guard = LOGGER_READY
        .lock()
        .map_err(|_| ScrcpyError::Other("logger state lock poisoned".to_string()))?;
    if *guard {
        return Ok(());
    }

    let level = map_level(max_level);
    // 仅安装 FlutterLogLayer：
    // - 统一把 Rust 日志回传到 Flutter 日志面板；
    // - 避免 stdout 与 Flutter 面板双份输出造成刷屏和诊断噪音。
    tracing_subscriber::registry()
        .with(LevelFilter::from_level(level))
        .with(FlutterLogLayer)
        .try_init()
        .map_err(|e| ScrcpyError::Other(format!("setup logger failed: {}", e)))?;

    *guard = true;
    info!("rust-scrcpy 新服务日志初始化完成, level={:?}", level);
    Ok(())
}

/// 通过 adb 列出在线设备。
///
/// 返回策略：
/// - 只保留 `state == device` 的条目；
/// - 型号、版本、分辨率在列表阶段不额外查询，避免慢查询阻塞接口。
pub async fn list_devices(adb_path: String) -> Result<Vec<DeviceInfo>> {
    let out = adb_execute(&adb_path, &["devices"]).await?;
    let mut devices = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") {
            continue;
        }
        if let Some((id, state)) = line.split_once('\t') {
            if state.trim() == "device" {
                devices.push(DeviceInfo {
                    device_id: id.trim().to_string(),
                    model: "Unknown".to_string(),
                    android_version: "Unknown".to_string(),
                    width: 0,
                    height: 0,
                    ip: None,
                });
            }
        }
    }
    Ok(devices)
}

/// 查询单设备详细信息。
///
/// 查询来源：
/// - 型号：`ro.product.model`
/// - 系统版本：`ro.build.version.release`
/// - 分辨率：`wm size`
///
/// 任一字段查询失败时降级为默认值，而不是整接口失败。
pub async fn get_device_info(adb_path: String, device_id: String) -> Result<DeviceInfo> {
    let model = adb_execute(&adb_path, &["-s", &device_id, "shell", "getprop ro.product.model"])
        .await
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    let android_version = adb_execute(
        &adb_path,
        &["-s", &device_id, "shell", "getprop ro.build.version.release"],
    )
    .await
    .unwrap_or_else(|_| "Unknown".to_string())
    .trim()
    .to_string();

    let size_raw = adb_execute(&adb_path, &["-s", &device_id, "shell", "wm size"])
        .await
        .unwrap_or_else(|_| String::new());
    let (width, height) = parse_wm_size(&size_raw).unwrap_or((0, 0));

    Ok(DeviceInfo {
        device_id,
        model,
        android_version,
        width,
        height,
        ip: None,
    })
}

/// 从 `wm size` 命令输出中提取宽高。
///
/// 兼容格式：
/// - `Physical size: 1080x2400`
/// - `1080x2400`
fn parse_wm_size(raw: &str) -> Option<(u32, u32)> {
    for line in raw.lines() {
        let line = line.trim();
        if let Some(idx) = line.find(':') {
            let size = line[idx + 1..].trim();
            if let Some((w, h)) = parse_size_pair(size) {
                return Some((w, h));
            }
        } else if let Some((w, h)) = parse_size_pair(line) {
            return Some((w, h));
        }
    }
    None
}

/// 解析 `WxH` 文本为宽高整数。
///
/// 返回 `None` 的场景：
/// - 不是合法数字；
/// - 宽或高为 0。
fn parse_size_pair(value: &str) -> Option<(u32, u32)> {
    let mut parts = value.split('x');
    let w = parts.next()?.trim().parse::<u32>().ok()?;
    let h = parts.next()?.trim().parse::<u32>().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// 创建会话（V1）。
///
/// 行为说明：
/// - 只注册配置并返回 `session_id`；
/// - 不会启动连接与解码线程；
/// - 默认使用硬解优先 + 原始渲染链路。
pub async fn create_session(config: SessionConfig) -> Result<String> {
    let session_id = new_session_id();
    let session = ApiSession {
        config,
        decoder_mode: DecoderMode::PreferHardware,
        render_pipeline_mode: RenderPipelineMode::Original,
        worker: None,
    };
    lock_sessions()?.insert(session_id.clone(), session);
    Ok(session_id)
}

/// 创建会话（V2）。
///
/// 与 V1 的差异：
/// - 显式接收解码模式与渲染模式；
/// - 其它行为与 V1 一致（仅注册，不启动）。
pub async fn create_session_v2(config: SessionConfigV2) -> Result<String> {
    let session_id = new_session_id();
    let session = ApiSession {
        config: config.base,
        decoder_mode: config.decoder_mode,
        render_pipeline_mode: config.render_pipeline_mode,
        worker: None,
    };
    lock_sessions()?.insert(session_id.clone(), session);
    Ok(session_id)
}

/// 启动会话。
///
/// 幂等语义：
/// - 若会话已在运行，则直接返回成功；
/// - 若会话存在但未运行，则拉起 worker。
pub async fn start_session(session_id: String) -> Result<()> {
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };

    if let Some(worker) = &session.worker {
        if worker.running.load(Ordering::Relaxed) {
            return Ok(());
        }
    }

    let worker = spawn_runtime_worker(
        session_id.clone(),
        session.config.clone(),
        session.decoder_mode,
        session.render_pipeline_mode,
    );
    session.worker = Some(worker);
    Ok(())
}

/// 停止会话。
///
/// 行为说明：
/// - 向 worker 发送 `Stop` 命令；
/// - 等待线程回收；
/// - 保留会话配置，可再次 `start_session`。
pub async fn stop_session(session_id: String) -> Result<()> {
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };

    if let Some(worker) = &mut session.worker {
        let _ = worker.tx.try_send(RuntimeCommand::Stop);
        worker.running.store(false, Ordering::Relaxed);
        if let Some(join) = worker.join.take() {
            let _ = join.join();
        }
    }
    session.worker = None;
    Ok(())
}

/// 销毁会话。
///
/// 与 `stop_session` 的区别：
/// - `dispose` 会从全局会话表移除该会话；
/// - 移除后该 `session_id` 不可再次使用。
pub async fn dispose_session(session_id: String) -> Result<()> {
    let mut sessions = lock_sessions()?;
    let Some(mut session) = sessions.remove(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };

    if let Some(worker) = &mut session.worker {
        let _ = worker.tx.try_send(RuntimeCommand::Stop);
        worker.running.store(false, Ordering::Relaxed);
        if let Some(join) = worker.join.take() {
            let _ = join.join();
        }
    }
    Ok(())
}

/// 投递触摸命令到会话 worker。
///
/// 该接口只负责命令投递，不保证设备端已经处理完成。
pub async fn send_touch(session_id: String, event: TouchEvent) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::Touch(event))
             .map_err(|e| ScrcpyError::Other(format!("send touch command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 投递按键命令到会话 worker。
pub async fn send_key(session_id: String, event: KeyEvent) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::Key(event))
             .map_err(|e| ScrcpyError::Other(format!("send key command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 投递滚轮命令到会话 worker。
pub async fn send_scroll(session_id: String, event: ScrollEvent) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::Scroll(event))
             .map_err(|e| ScrcpyError::Other(format!("send scroll command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 投递文本输入命令到会话 worker。
pub async fn send_text(session_id: String, text: String) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::Text(text))
             .map_err(|e| ScrcpyError::Other(format!("send text command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 投递系统语义按键命令到会话 worker。
pub async fn send_system_key(session_id: String, key: SystemKey) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::SystemKey(key))
             .map_err(|e| ScrcpyError::Other(format!("send system key command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 投递剪贴板设置命令到会话 worker。
///
/// `paste=true` 时，设备端会尝试立刻执行一次粘贴动作。
pub async fn set_clipboard(session_id: String, text: String, paste: bool) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::Clipboard { text, paste })
             .map_err(|e| ScrcpyError::Other(format!("send clipboard command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 投递方向模式命令到会话 worker。
///
/// 当前实现会转为事件通知，方便上层统一消费方向状态变化。
pub async fn set_orientation_mode(session_id: String, mode: OrientationMode) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::SetOrientation(mode))
             .map_err(|e| ScrcpyError::Other(format!("send orientation command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 投递 IDR 请求命令到会话 worker。
///
/// 用于在解码异常后请求关键帧，加快画面恢复。
pub async fn request_idr(session_id: String) -> Result<()> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .tx
            .try_send(RuntimeCommand::RequestIdr)
             .map_err(|e| ScrcpyError::Other(format!("send request idr command failed: {}", e)))
    } else {
        Err(ScrcpyError::Other("session runtime not started".to_string()))
    }
}

/// 获取会话统计快照。
///
/// 行为说明：
/// - 已运行会话返回最近一次 worker 刷新的统计值；
/// - 未运行会话返回默认统计对象，而不是错误。
pub async fn get_session_stats(session_id: String) -> Result<SessionStats> {
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        return Err(invalid_session_error(&session_id));
    };
    if let Some(worker) = &session.worker {
        worker
            .stats
            .lock()
            .map(|s| s.clone())
            .map_err(|_| ScrcpyError::Other("session stats lock poisoned".to_string()))
    } else {
        Ok(default_stats())
    }
}



















