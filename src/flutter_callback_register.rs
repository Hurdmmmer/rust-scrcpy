//! Windows Runner 原生回调注册与分发模块。
//!
//! 设计约束：
//! 1. 本模块仅服务 Rust -> C++ Runner 的 C ABI 回调链路；
//! 2. 本模块不进入 FRB 暴露面，避免 `*mut c_void` 进入 Dart 桥接；
//! 3. 保持导出函数名稳定，避免破坏现有 Windows Runner 链路。
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// 与 Runner 侧约定的像素格式常量：
/// - 4: BGRA32
/// - 5: RGBA32
pub const PIXEL_FORMAT_RGBA32: u32 = 5;

/// V2 回调：直接推送 CPU 像素内存（data + 元信息）。
type V2FrameCallback = extern "C" fn(
    user_data: *mut c_void,
    frame_id: u64,
    data: *const u8,
    data_len: usize,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
    generation: u64,
    pts: i64,
);

/// V1 回调：仅推送 GPU 共享句柄元信息（不传像素内存）。
type V1FrameCallback = extern "C" fn(
    user_data: *mut c_void,
    handle: i64,
    width: u32,
    height: u32,
    generation: u64,
    pts: i64,
);

/// SessionEvent 回调：推送会话事件 JSON。
///
/// 参数约定：
/// - session_id/session_id_len: 会话 ID UTF-8 字节；
/// - event_json/event_json_len: SessionEvent 的 JSON UTF-8 字节。
type SessionEventCallback = extern "C" fn(
    user_data: *mut c_void,
    session_id: *const u8,
    session_id_len: usize,
    event_json: *const u8,
    event_json_len: usize,
);

/// RustLog 回调：推送 Rust tracing 日志。
///
/// 参数约定：
/// - level/level_len: 日志级别文本（UTF-8，如 INFO/WARN/ERROR）；
/// - message/message_len: 日志正文（UTF-8）。
type RustLogCallback = extern "C" fn(
    user_data: *mut c_void,
    level: *const u8,
    level_len: usize,
    message: *const u8,
    message_len: usize,
);

#[derive(Clone, Copy)]
struct V2CallbackHolder {
    callback: Option<V2FrameCallback>,
    /// 由调用方透传的上下文指针（C++ Runner 实例）。
    user_data: usize,
}

static V2_CALLBACK: Lazy<Mutex<V2CallbackHolder>> = Lazy::new(|| {
    Mutex::new(V2CallbackHolder {
        callback: None,
        user_data: 0,
    })
});

#[derive(Clone, Copy)]
struct V1CallbackHolder {
    callback: Option<V1FrameCallback>,
    /// 由调用方透传的上下文指针（C++ Runner 实例）。
    user_data: usize,
}

static V1_CALLBACK: Lazy<Mutex<V1CallbackHolder>> = Lazy::new(|| {
    Mutex::new(V1CallbackHolder {
        callback: None,
        user_data: 0,
    })
});

#[derive(Clone, Copy)]
struct SessionEventCallbackHolder {
    callback: Option<SessionEventCallback>,
    /// 由调用方透传的上下文指针（C++ Runner 实例）。
    user_data: usize,
}

static SESSION_EVENT_CALLBACK: Lazy<Mutex<SessionEventCallbackHolder>> = Lazy::new(|| {
    Mutex::new(SessionEventCallbackHolder {
        callback: None,
        user_data: 0,
    })
});

#[derive(Clone, Copy)]
struct RustLogCallbackHolder {
    callback: Option<RustLogCallback>,
    /// 由调用方透传的上下文指针（C++ Runner 实例）。
    user_data: usize,
}

static RUST_LOG_CALLBACK: Lazy<Mutex<RustLogCallbackHolder>> = Lazy::new(|| {
    Mutex::new(RustLogCallbackHolder {
        callback: None,
        user_data: 0,
    })
});

pub fn next_frame_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// 注册 V2 帧回调（由 Windows Runner 调用一次）。
///
/// 回调触发线程：Rust 解码工作线程。
#[no_mangle]
pub extern "C" fn rs_register_v2_frame_callback(
    callback: Option<V2FrameCallback>,
    user_data: *mut c_void,
) -> bool {
    let Ok(mut guard) = V2_CALLBACK.lock() else {
        return false;
    };
    guard.callback = callback;
    guard.user_data = user_data as usize;
    true
}

/// 注册 V1 帧回调（DXGI 元信息回调）。
///
/// 回调触发线程：Rust 解码工作线程。
#[no_mangle]
pub extern "C" fn rs_register_v1_frame_callback(
    callback: Option<V1FrameCallback>,
    user_data: *mut c_void,
) -> bool {
    let Ok(mut guard) = V1_CALLBACK.lock() else {
        return false;
    };
    guard.callback = callback;
    guard.user_data = user_data as usize;
    true
}

/// 注册 SessionEvent 回调（由 Windows Runner 调用一次）。
///
/// 回调触发线程：Rust 运行时 worker 线程。
#[no_mangle]
pub extern "C" fn rs_register_session_event_callback(
    callback: Option<SessionEventCallback>,
    user_data: *mut c_void,
) -> bool {
    let Ok(mut guard) = SESSION_EVENT_CALLBACK.lock() else {
        return false;
    };
    guard.callback = callback;
    guard.user_data = user_data as usize;
    true
}

/// 注册 RustLog 回调（由 Windows Runner 调用一次）。
///
/// 回调触发线程：Rust 运行时线程（tracing 产生日志时）。
#[no_mangle]
pub extern "C" fn rs_register_rust_log_callback(
    callback: Option<RustLogCallback>,
    user_data: *mut c_void,
) -> bool {
    let Ok(mut guard) = RUST_LOG_CALLBACK.lock() else {
        return false;
    };
    guard.callback = callback;
    guard.user_data = user_data as usize;
    true
}

/// 向外部（Runner）推送一条 V2 新帧通知（直接携带像素内存与元数据）。
pub fn notify_v2_frame_raw(
    frame_id: u64,
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
    generation: u64,
    pts: i64,
) {
    // 回调指针先复制到栈，避免持锁期间执行外部代码。
    let (cb, user_data) = {
        let Ok(guard) = V2_CALLBACK.lock() else {
            return;
        };
        (guard.callback, guard.user_data as *mut c_void)
    };

    if let Some(callback) = cb {
        callback(
            user_data,
            frame_id,
            data.as_ptr(),
            data.len(),
            width,
            height,
            stride,
            pixel_format,
            generation,
            pts,
        );
    }
}

/// 向外部（Runner）推送一条 V1 句柄帧通知。
pub fn notify_v1_frame(
    handle: i64,
    width: u32,
    height: u32,
    generation: u64,
    pts: i64,
) {
    // 回调指针先复制到栈，避免持锁期间执行外部代码。
    let (cb, user_data) = {
        let Ok(guard) = V1_CALLBACK.lock() else {
            return;
        };
        (guard.callback, guard.user_data as *mut c_void)
    };

    if let Some(callback) = cb {
        callback(user_data, handle, width, height, generation, pts);
    }
}

/// 向外部（Runner）推送一条会话事件通知。
///
/// `event_json` 要求为 UTF-8 JSON 字节（通常由 `serde_json::to_vec` 生成）。
pub fn notify_session_event(session_id: &str, event_json: &[u8]) {
    let (cb, user_data) = {
        let Ok(guard) = SESSION_EVENT_CALLBACK.lock() else {
            return;
        };
        (guard.callback, guard.user_data as *mut c_void)
    };

    if let Some(callback) = cb {
        callback(
            user_data,
            session_id.as_ptr(),
            session_id.len(),
            event_json.as_ptr(),
            event_json.len(),
        );
    }
}

/// 向外部（Runner）推送一条 Rust tracing 日志。
///
/// `level` 与 `message` 要求为 UTF-8 文本。
pub fn notify_rust_log(level: &str, message: &str) {
    let (cb, user_data) = {
        let Ok(guard) = RUST_LOG_CALLBACK.lock() else {
            return;
        };
        (guard.callback, guard.user_data as *mut c_void)
    };

    if let Some(callback) = cb {
        callback(
            user_data,
            level.as_ptr(),
            level.len(),
            message.as_ptr(),
            message.len(),
        );
    }
}


