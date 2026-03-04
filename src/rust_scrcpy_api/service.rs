//! API 服务层：承载 FRB 暴露的业务入口与会话表管理。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use once_cell::sync::Lazy;
use tracing::{debug, info, warn, Level};
use tracing_subscriber::util::SubscriberInitExt;

use crate::adb::AdbClient;
use crate::error::{Result, ScrcpyError};
use crate::scrcpy::control::{KeyEvent, ScrollEvent, TouchEvent};

use super::runtime::{RealSessionRuntime, SessionRuntime};
use super::{
    DeviceInfo, LogLevel, OrientationMode, SessionConfig, SessionConfigV2, SessionStats,
    SystemKey,
};

/// API 层内部会话状态。
struct ApiSession {
    config: SessionConfig,
    runtime: Box<dyn SessionRuntime + Send>,
}

/// 会话 ID 递增计数器。
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
/// 全局会话表。
static API_SESSIONS: Lazy<Mutex<HashMap<String, ApiSession>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// 日志初始化标记，避免 DLL 侧重复安装 subscriber。
static LOGGER_READY: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

/// 获取全局会话表互斥锁。
///
/// 约束：
/// - 返回值生命周期受全局静态会话表约束；
/// - 调用方应尽量缩短持锁时间，避免影响其它 FFI 请求。
fn lock_sessions() -> Result<MutexGuard<'static, HashMap<String, ApiSession>>> {
    API_SESSIONS
        .lock()
        .map_err(|_| ScrcpyError::Other("api session map poisoned".to_string()))
}

/// 生成新的会话 ID。
///
/// 格式：`sess-{n}`，其中 `n` 为递增计数。
fn new_session_id() -> String {
    format!("sess-{}", NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed))
}

/// 统一构造“会话不存在”错误。
fn invalid_session_error(session_id: &str) -> ScrcpyError {
    ScrcpyError::Other(format!("invalid session id: {}", session_id))
}

/// 将对外日志级别映射到 `tracing` 级别。
fn map_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Trace => Level::TRACE,
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Info => Level::INFO,
        LogLevel::Warn => Level::WARN,
        LogLevel::Error => Level::ERROR,
    }
}

/// 初始化 DLL 日志系统。
///
/// 行为：
/// - 仅首次调用生效，后续调用直接返回 `Ok(())`；
/// - 日志会包含线程、文件与行号，便于跨线程问题定位。
pub async fn setup_logger(max_level: LogLevel) -> Result<()> {
    let mut guard = LOGGER_READY
        .lock()
        .map_err(|_| ScrcpyError::Other("logger state lock poisoned".to_string()))?;
    if *guard {
        return Ok(());
    }

    let level = map_level(max_level);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .finish()
        .try_init()
        .map_err(|e| ScrcpyError::Other(format!("setup logger failed: {}", e)))?;

    *guard = true;
    info!("rust-scrcpy dll logger initialized, level={:?}", level);
    Ok(())
}

/// 列出当前 ADB 在线设备。
///
/// 返回：
/// - 仅保证 `device_id` 可用；
/// - 型号/系统版本/分辨率为占位值，需配合 `get_device_info()` 获取详情。
pub async fn list_devices(adb_path: String) -> Result<Vec<DeviceInfo>> {
    debug!("list_devices called, adb_path={}", adb_path);
    let adb = AdbClient::new(PathBuf::from(adb_path));
    let ids = adb.list_devices().await?;
    info!("list_devices success, count={}", ids.len());

    Ok(ids
        .into_iter()
        .map(|id| DeviceInfo {
            device_id: id,
            model: "Unknown".to_string(),
            android_version: "Unknown".to_string(),
            width: 0,
            height: 0,
            ip: None,
        })
        .collect())
}

/// 读取单个设备详情。
///
/// 数据来源：
/// - 通过 ADB shell 读取品牌、型号、Android 版本与 `wm size`；
/// - 若部分字段不可用，按降级策略填充默认值。
pub async fn get_device_info(adb_path: String, device_id: String) -> Result<DeviceInfo> {
    debug!(
        "get_device_info called, adb_path={}, device_id={}",
        adb_path, device_id
    );
    let adb = AdbClient::new(PathBuf::from(adb_path));

    let brand = adb
        .shell(&device_id, "getprop ro.product.brand")
        .await
        .unwrap_or_else(|_| String::new())
        .trim()
        .to_string();

    let manufacturer = adb
        .shell(&device_id, "getprop ro.product.manufacturer")
        .await
        .unwrap_or_else(|_| String::new())
        .trim()
        .to_string();

    let model = adb
        .shell(&device_id, "getprop ro.product.model")
        .await
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    let android_version = adb
        .shell(&device_id, "getprop ro.build.version.release")
        .await
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    let sdk = adb
        .shell(&device_id, "getprop ro.build.version.sdk")
        .await
        .unwrap_or_else(|_| String::new())
        .trim()
        .to_string();

    let size_raw = adb
        .shell(&device_id, "wm size")
        .await
        .unwrap_or_else(|_| String::new());
    let (width, height) = parse_wm_size(&size_raw).unwrap_or((0, 0));

    let model = if model.eq_ignore_ascii_case("unknown") || model.is_empty() {
        if !manufacturer.is_empty() {
            manufacturer.clone()
        } else if !brand.is_empty() {
            brand.clone()
        } else {
            model
        }
    } else if !brand.is_empty() && !model.to_lowercase().starts_with(&brand.to_lowercase()) {
        format!("{} {}", brand, model)
    } else {
        model
    };

    let android_version = if !android_version.eq_ignore_ascii_case("unknown") && !sdk.is_empty() {
        format!("{} (SDK {})", android_version, sdk)
    } else {
        android_version
    };

    Ok(DeviceInfo {
        device_id,
        model,
        android_version,
        width,
        height,
        ip: None,
    })
}

/// 解析 `wm size` 输出，返回 `(width, height)`。
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

/// 解析 `1080x2400` 形式的尺寸字符串。
fn parse_size_pair(value: &str) -> Option<(u32, u32)> {
    let mut parts = value.split('x');
    let w = parts.next()?.trim().parse::<u32>().ok()?;
    let h = parts.next()?.trim().parse::<u32>().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// 创建会话（仅创建，不启动）。
pub async fn create_session(config: SessionConfig) -> Result<String> {
    info!(
        "create_session called, device_id={}, ports={}/{}",
        config.device_id, config.video_port, config.control_port
    );

    let session_id = new_session_id();
    let session = ApiSession {
        config: config.clone(),
        // 传入 session_id，供 Rust->Runner 会话事件回调链路区分归属。
        runtime: Box::new(RealSessionRuntime::new(session_id.clone(), config)),
    };

    lock_sessions()?.insert(session_id.clone(), session);
    info!("create_session success, session_id={}", session_id);
    Ok(session_id)
}

/// 创建会话（V2：支持解码模式与渲染模式配置，旧 API 不受影响）。
pub async fn create_session_v2(config: SessionConfigV2) -> Result<String> {
    info!(
        "create_session_v2 called, device_id={}, ports={}/{} mode={:?}/{:?}",
        config.base.device_id,
        config.base.video_port,
        config.base.control_port,
        config.render_pipeline_mode,
        config.decoder_mode
    );

    let session_id = new_session_id();
    let session = ApiSession {
        config: config.base.clone(),
        runtime: Box::new(RealSessionRuntime::new_with_options(
            session_id.clone(),
            config.base,
            config.decoder_mode,
            config.render_pipeline_mode,
        )),
    };

    lock_sessions()?.insert(session_id.clone(), session);
    info!("create_session_v2 success, session_id={}", session_id);
    Ok(session_id)
}

/// 启动会话。
pub async fn start_session(session_id: String) -> Result<()> {
    info!("start_session called, session_id={}", session_id);
    // 直接在 map 内部就地操作，避免 remove-then-reinsert 的会话丢失窗口。
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("start_session failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    if session.runtime.is_running() {
        warn!("start_session ignored: already running, session_id={}", session_id);
        return Ok(());
    }
    session.runtime.start()?;
    info!("start_session success, session_id={}", session_id);
    Ok(())
}

/// 停止会话。
pub async fn stop_session(session_id: String) -> Result<()> {
    let started = Instant::now();
    info!("stop_session called, session_id={}", session_id);
    // 直接在 map 内部就地操作，避免 remove-then-reinsert 的会话丢失窗口。
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("stop_session failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    if !session.runtime.is_running() {
        warn!("stop_session ignored: not running, session_id={}", session_id);
        return Ok(());
    }
    session.runtime.stop()?;
    info!(
        "stop_session success, session_id={}, cost={}ms",
        session_id,
        started.elapsed().as_millis()
    );
    Ok(())
}

/// 销毁会话并移除状态。
pub async fn dispose_session(session_id: String) -> Result<()> {
    let started = Instant::now();
    info!("dispose_session called, session_id={}", session_id);
    let mut session = {
        let mut sessions = lock_sessions()?;
        let Some(session) = sessions.remove(&session_id) else {
            warn!("dispose_session failed: invalid session_id={}", session_id);
            return Err(invalid_session_error(&session_id));
        };
        session
    };
    let stop_started = Instant::now();
    let _ = session.runtime.stop();
    info!(
        "dispose_session stop done, session_id={}, stage_cost={}ms",
        session_id,
        stop_started.elapsed().as_millis()
    );
    info!(
        "dispose_session success, session_id={}, cost={}ms",
        session_id,
        started.elapsed().as_millis()
    );
    Ok(())
}

// 已移除轮询暴露接口（stream_texture_frames / stream_session_events）。
// 会话/帧通知统一通过 callback 链路分发到 Flutter。

/// 向目标会话发送触摸事件。
pub async fn send_touch(session_id: String, event: TouchEvent) -> Result<()> {
    // debug!("send_touch called, session_id={}", session_id);

    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("send_touch failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    session.runtime.send_touch(event)
}

/// 向目标会话发送按键事件。
pub async fn send_key(session_id: String, event: KeyEvent) -> Result<()> {
    debug!("send_key called, session_id={}", session_id);
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("send_key failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    session.runtime.send_key(event)
}

/// 向目标会话发送滚轮事件。
pub async fn send_scroll(session_id: String, event: ScrollEvent) -> Result<()> {
    debug!("send_scroll called, session_id={}", session_id);
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("send_scroll failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    session.runtime.send_scroll(event)
}

/// 向目标会话发送文本输入。
pub async fn send_text(session_id: String, text: String) -> Result<()> {
    debug!("send_text called, session_id={}", session_id);
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("send_text failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    session.runtime.send_text(text)
}

/// 向目标会话发送系统按键语义事件（Home/Back/音量等）。
pub async fn send_system_key(session_id: String, key: SystemKey) -> Result<()> {
    debug!("send_system_key called, session_id={}, key={:?}", session_id, key);
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("send_system_key failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };

    session.runtime.send_system_key(key)
}

/// 设置设备剪贴板内容。
pub async fn set_clipboard(session_id: String, text: String, paste: bool) -> Result<()> {
    debug!("set_clipboard called, session_id={}", session_id);
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("set_clipboard failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    session.runtime.set_clipboard(text, paste)
}

/// 设置会话方向模式（主动旋转入口）。
pub async fn set_orientation_mode(session_id: String, mode: OrientationMode) -> Result<()> {
    info!(
        "set_orientation_mode called, session_id={}, mode={:?}",
        session_id, mode
    );
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!(
            "set_orientation_mode failed: invalid session_id={}",
            session_id
        );
        return Err(invalid_session_error(&session_id));
    };
    session.runtime.set_orientation_mode(mode)?;
    Ok(())
}

/// 请求会话尽快输出关键帧（IDR）。
pub async fn request_idr(session_id: String) -> Result<()> {
    debug!("request_idr called, session_id={}", session_id);
    let mut sessions = lock_sessions()?;
    let Some(session) = sessions.get_mut(&session_id) else {
        warn!("request_idr failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    session.runtime.request_idr()?;
    Ok(())
}

/// 获取会话统计快照。
pub async fn get_session_stats(session_id: String) -> Result<SessionStats> {
    debug!("get_session_stats called, session_id={}", session_id);
    let sessions = lock_sessions()?;
    let Some(session) = sessions.get(&session_id) else {
        warn!("get_session_stats failed: invalid session_id={}", session_id);
        return Err(invalid_session_error(&session_id));
    };
    let _ = &session.config;
    Ok(session.runtime.stats())
}
