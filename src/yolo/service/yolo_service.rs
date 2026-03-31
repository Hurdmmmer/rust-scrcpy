use std::sync::{Condvar, Mutex, Once};

use once_cell::sync::Lazy;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::frb_generated::StreamSink;
use crate::gh_common::model::{YoloConfig, YoloFrameResult};
use crate::gh_common::{Result, ScrcpyError};
#[cfg(target_os = "windows")]
use crate::scrcpy::decode_core::gpu_direct_output::D3D11Context;
use crate::yolo::config::yolo_config::validate_yolo_config;
use crate::yolo::runtime::yolo_runtime::YoloRuntime;
#[cfg(target_os = "windows")]
use windows::core::Interface;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

/// YOLO 推理结果广播通道。
static YOLO_RESULT_BUS: Lazy<broadcast::Sender<YoloFrameResult>> = Lazy::new(|| {
    let (tx, _rx) = broadcast::channel::<YoloFrameResult>(512);
    tx
});

/// YOLO 运行时全局实例。
static YOLO_RUNTIME: Lazy<Mutex<YoloRuntime>> = Lazy::new(|| Mutex::new(YoloRuntime::default()));

/// 单帧推理任务。
#[derive(Debug, Clone)]
enum InferTask {
    /// CPU BGRA 帧推理任务。
    CpuBgra {
        session_id: String,
        frame_id: u64,
        width: u32,
        height: u32,
        stride: u32,
        bgra: Vec<u8>,
    },
    /// GPU 共享纹理句柄推理任务（V1 链路）。
    GpuShared {
        session_id: String,
        frame_id: u64,
        handle: i64,
        width: u32,
        height: u32,
    },
}

/// 最新帧队列状态（覆盖式，无界等待）。
#[derive(Debug, Default)]
struct LatestInferState {
    latest: Option<InferTask>,
    dropped_on_overwrite: u64,
}

/// 最新帧覆盖队列：
/// - 解码线程只写入“最新任务”，不阻塞；
/// - 推理线程按顺序消费最新任务，旧任务会被覆盖丢弃。
#[derive(Debug, Default)]
struct LatestInferQueue {
    state: Mutex<LatestInferState>,
    cv: Condvar,
}

impl LatestInferQueue {
    /// 推送最新推理任务。
    ///
    /// 说明：若队列已有未消费任务，会被新任务覆盖（旧任务计入丢弃统计）。
    fn push_latest(&self, task: InferTask) -> u64 {
        let mut guard = match self.state.lock() {
            Ok(v) => v,
            Err(_) => return 0,
        };
        if guard.latest.is_some() {
            guard.dropped_on_overwrite = guard.dropped_on_overwrite.saturating_add(1);
        }
        guard.latest = Some(task);
        let dropped = guard.dropped_on_overwrite;
        self.cv.notify_one();
        dropped
    }

    /// 阻塞等待下一条任务。
    fn pop_wait(&self) -> Option<InferTask> {
        let mut guard = self.state.lock().ok()?;
        loop {
            if let Some(task) = guard.latest.take() {
                return Some(task);
            }
            guard = self.cv.wait(guard).ok()?;
        }
    }
}

/// YOLO 推理任务队列（全局单消费者）。
static YOLO_INFER_QUEUE: Lazy<LatestInferQueue> = Lazy::new(LatestInferQueue::default);
/// YOLO 推理工作线程启动控制。
static YOLO_INFER_WORKER_ONCE: Once = Once::new();
/// GPU 共享纹理读取器（仅 Windows）。
#[cfg(target_os = "windows")]
static YOLO_GPU_READER: Lazy<Mutex<Option<GpuSharedFrameReader>>> = Lazy::new(|| Mutex::new(None));

/// 共享纹理读回的 staging 缓冲信息（仅 Windows）。
#[cfg(target_os = "windows")]
#[derive(Debug)]
struct StagingSlot {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

/// GPU 共享纹理读取器（仅 Windows）。
///
/// 作用：
/// - 打开来自渲染链路的共享纹理句柄；
/// - 复制到 CPU 可读 staging 纹理；
/// - 读回 BGRA 数据给 YOLO 推理。
#[cfg(target_os = "windows")]
struct GpuSharedFrameReader {
    ctx: D3D11Context,
    staging: Option<StagingSlot>,
}

#[cfg(target_os = "windows")]
impl GpuSharedFrameReader {
    /// 创建读取器实例。
    fn new() -> Result<Self> {
        let ctx = D3D11Context::new()
            .map_err(|e| ScrcpyError::Other(format!("create yolo d3d11 context failed: {e}")))?;
        Ok(Self { ctx, staging: None })
    }

    /// 从共享纹理句柄读取 BGRA。
    ///
    /// 参数：
    /// - `handle`：共享纹理句柄；
    /// - `width`：期望宽度；
    /// - `height`：期望高度。
    fn read_bgra(&mut self, handle: i64, width: u32, height: u32) -> Result<Vec<u8>> {
        let src_tex = unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self.ctx
                .device()
                .OpenSharedResource::<_, ID3D11Texture2D>(
                    HANDLE(handle as usize as *mut core::ffi::c_void),
                    &mut tex,
                )
                .map_err(|e| ScrcpyError::Other(format!("open shared texture failed: {e}")))?;
            tex.ok_or_else(|| ScrcpyError::Other("open shared texture returned null".to_string()))?
        };

        let staging = self.ensure_staging(&src_tex, width, height)?;
        let src_res: ID3D11Resource = src_tex
            .cast()
            .map_err(|e| ScrcpyError::Other(format!("cast src texture failed: {e}")))?;
        let dst_res: ID3D11Resource = staging
            .texture
            .cast()
            .map_err(|e| ScrcpyError::Other(format!("cast staging texture failed: {e}")))?;

        unsafe {
            self.ctx
                .immediate_context()
                .CopyResource(&dst_res, &src_res);
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx
                .immediate_context()
                .Map(&dst_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| ScrcpyError::Other(format!("map staging texture failed: {e}")))?;
        }

        let row_pitch = mapped.RowPitch as usize;
        let row_bytes = (width as usize) * 4;
        let total_bytes = row_bytes * (height as usize);
        let mut out = Vec::<u8>::with_capacity(total_bytes);
        unsafe {
            let base = mapped.pData as *const u8;
            for y in 0..(height as usize) {
                let row_ptr = base.add(y * row_pitch);
                let row = std::slice::from_raw_parts(row_ptr, row_bytes);
                out.extend_from_slice(row);
            }
            self.ctx.immediate_context().Unmap(&dst_res, 0);
        }
        Ok(out)
    }

    /// 确保 staging 纹理可用且尺寸匹配。
    fn ensure_staging(
        &mut self,
        src_tex: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<&StagingSlot> {
        let need_recreate = match &self.staging {
            Some(s) => s.width != width || s.height != height,
            None => true,
        };
        if need_recreate {
            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                src_tex.GetDesc(&mut src_desc);
            }
            src_desc.Usage = D3D11_USAGE_STAGING;
            src_desc.BindFlags = 0;
            src_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            src_desc.MiscFlags = 0;
            src_desc.Width = width;
            src_desc.Height = height;
            src_desc.MipLevels = 1;
            src_desc.ArraySize = 1;
            let mut staging_tex: Option<ID3D11Texture2D> = None;
            unsafe {
                self.ctx
                    .device()
                    .CreateTexture2D(&src_desc, None, Some(&mut staging_tex))
                    .map_err(|e| {
                        ScrcpyError::Other(format!("create staging texture failed: {e}"))
                    })?;
            }
            self.staging = Some(StagingSlot {
                texture: staging_tex.ok_or_else(|| {
                    ScrcpyError::Other("create staging texture returned null".to_string())
                })?,
                width,
                height,
            });
        }
        self.staging
            .as_ref()
            .ok_or_else(|| ScrcpyError::Other("staging texture missing".to_string()))
    }
}

/// 从 GPU 共享纹理句柄读取 BGRA（仅 Windows）。
#[cfg(target_os = "windows")]
fn readback_gpu_shared_bgra(handle: i64, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut guard = YOLO_GPU_READER
        .lock()
        .map_err(|_| ScrcpyError::Other("gpu reader lock poisoned".to_string()))?;
    if guard.is_none() {
        *guard = Some(GpuSharedFrameReader::new()?);
    }
    guard
        .as_mut()
        .ok_or_else(|| ScrcpyError::Other("gpu reader init failed".to_string()))?
        .read_bgra(handle, width, height)
}

/// 非 Windows 平台占位：当前不支持共享纹理读回。
#[cfg(not(target_os = "windows"))]
fn readback_gpu_shared_bgra(_handle: i64, _width: u32, _height: u32) -> Result<Vec<u8>> {
    Err(ScrcpyError::Other(
        "gpu shared readback is not supported on this platform".to_string(),
    ))
}

/// 确保异步推理线程已启动（仅启动一次）。
fn ensure_infer_worker_started() {
    YOLO_INFER_WORKER_ONCE.call_once(|| {
        std::thread::Builder::new()
            .name("yolo-infer-worker".to_string())
            .spawn(|| {
                info!("[YOLO服务] 推理线程已启动（最新帧覆盖模式）");
                loop {
                    let Some(task) = YOLO_INFER_QUEUE.pop_wait() else {
                        warn!("[YOLO服务] 推理队列异常结束，线程退出");
                        break;
                    };
                    match task {
                        InferTask::CpuBgra {
                            session_id,
                            frame_id,
                            width,
                            height,
                            stride,
                            bgra,
                        } => {
                            let mut rt = match YOLO_RUNTIME.lock() {
                                Ok(v) => v,
                                Err(_) => {
                                    warn!("[YOLO服务] 运行时锁异常，跳过当前推理任务");
                                    continue;
                                }
                            };
                            let result = rt.infer_bgra_if_needed(
                                &session_id,
                                frame_id,
                                &bgra,
                                width,
                                height,
                                stride,
                            );
                            drop(rt);
                            match result {
                                Ok(Some(event)) => {
                                    let _ = YOLO_RESULT_BUS.send(event);
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    warn!(
                                        "[YOLO服务] 推理任务失败：session_id={}, frame_id={}, err={}",
                                        session_id, frame_id, e
                                    );
                                }
                            }
                        }
                        InferTask::GpuShared {
                            session_id,
                            frame_id,
                            handle,
                            width,
                            height,
                        } => {
                            let bgra = match readback_gpu_shared_bgra(handle, width, height) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(
                                        "[YOLO服务] 共享纹理读回失败：session_id={}, frame_id={}, handle={}, err={}",
                                        session_id, frame_id, handle, e
                                    );
                                    continue;
                                }
                            };
                            let mut rt = match YOLO_RUNTIME.lock() {
                                Ok(v) => v,
                                Err(_) => {
                                    warn!("[YOLO服务] 运行时锁异常，跳过当前推理任务");
                                    continue;
                                }
                            };
                            let result = rt.infer_bgra_if_needed(
                                &session_id,
                                frame_id,
                                &bgra,
                                width,
                                height,
                                width.saturating_mul(4),
                            );
                            drop(rt);
                            match result {
                                Ok(Some(event)) => {
                                    let _ = YOLO_RESULT_BUS.send(event);
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    warn!(
                                        "[YOLO服务] GPU任务推理失败：session_id={}, frame_id={}, err={}",
                                        session_id, frame_id, e
                                    );
                                }
                            }
                        }
                    }
                }
            })
            .map_err(|e| {
                warn!("[YOLO服务] 推理线程启动失败：err={}", e);
                e
            })
            .ok();
    });
}

/// 初始化 YOLO 配置与运行时。
///
/// 参数：
/// - `config`：YOLO 推理配置（仅硬件后端）。
///
/// 返回：
/// - `Ok(())`：初始化成功；
/// - `Err`：初始化失败，错误可透传上层。
pub async fn init_yolo(config: YoloConfig) -> Result<()> {
    info!(
        "[YOLO服务] 初始化请求：model_path={}, provider={:?}, input={}x{}",
        config.model_path, config.provider, config.input_width, config.input_height
    );
    validate_yolo_config(&config)?;
    let mut rt = YOLO_RUNTIME
        .lock()
        .map_err(|_| ScrcpyError::Other("yolo runtime lock poisoned".to_string()))?;
    rt.init(config)?;
    info!("[YOLO服务] 初始化成功");
    Ok(())
}

/// 运行中更新 YOLO 推理配置。
///
/// 参数：
/// - `config`：新的 YOLO 推理配置（仅硬件后端）。
///
/// 返回：
/// - `Ok(())`：更新成功；
/// - `Err`：更新失败，错误可透传上层。
pub async fn update_yolo_config(config: YoloConfig) -> Result<()> {
    info!(
        "[YOLO服务] 更新配置请求：model_path={}, provider={:?}, input={}x{}",
        config.model_path, config.provider, config.input_width, config.input_height
    );
    validate_yolo_config(&config)?;
    let mut rt = YOLO_RUNTIME
        .lock()
        .map_err(|_| ScrcpyError::Other("yolo runtime lock poisoned".to_string()))?;
    rt.update_config(config)?;
    info!("[YOLO服务] 更新配置成功");
    Ok(())
}

/// 设置会话级 YOLO 开关。
///
/// 参数：
/// - `session_id`：会话 ID；
/// - `enabled`：是否启用。
///
/// 返回：
/// - `Ok(())`：设置成功；
/// - `Err`：设置失败。
pub async fn set_yolo_enabled(session_id: String, enabled: bool) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(ScrcpyError::Other("session id is empty".to_string()));
    }
    let mut rt = YOLO_RUNTIME
        .lock()
        .map_err(|_| ScrcpyError::Other("yolo runtime lock poisoned".to_string()))?;
    rt.set_session_enabled(session_id.clone(), enabled);
    info!(
        "[YOLO服务] 会话开关更新：session_id={}, enabled={}",
        session_id, enabled
    );
    Ok(())
}

/// 执行一次会话帧推理并发布结果（供解码链路调用）。
///
/// 参数：
/// - `session_id`：会话 ID；
/// - `frame_id`：帧 ID；
/// - `bgra`：BGRA 帧数据；
/// - `width`：帧宽；
/// - `height`：帧高；
/// - `stride`：行步长字节数。
///
/// 返回：
/// - `Ok(())`：调用流程完成（即使因限频跳过也返回成功）；
/// - `Err`：运行时锁或推理执行失败。
pub fn infer_and_publish_frame(
    session_id: &str,
    frame_id: u64,
    bgra: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(ScrcpyError::Other("session id is empty".to_string()));
    }
    if width == 0 || height == 0 || stride == 0 {
        return Err(ScrcpyError::Other("invalid frame geometry".to_string()));
    }
    ensure_infer_worker_started();
    let dropped = YOLO_INFER_QUEUE.push_latest(InferTask::CpuBgra {
        session_id: session_id.to_string(),
        frame_id,
        width,
        height,
        stride,
        bgra: bgra.to_vec(),
    });
    if dropped > 0 && dropped % 120 == 0 {
        debug!("[YOLO服务] 推理队列覆盖丢帧累计={}", dropped);
    }
    Ok(())
}

/// 提交共享纹理句柄推理任务（V1 GpuShared 链路）。
///
/// 参数：
/// - `session_id`：会话 ID；
/// - `frame_id`：帧 ID；
/// - `handle`：共享纹理句柄；
/// - `width`：帧宽；
/// - `height`：帧高。
pub fn infer_and_publish_gpu_frame(
    session_id: &str,
    frame_id: u64,
    handle: i64,
    width: u32,
    height: u32,
) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(ScrcpyError::Other("session id is empty".to_string()));
    }
    if handle == 0 || width == 0 || height == 0 {
        return Err(ScrcpyError::Other("invalid gpu frame geometry".to_string()));
    }
    ensure_infer_worker_started();
    let dropped = YOLO_INFER_QUEUE.push_latest(InferTask::GpuShared {
        session_id: session_id.to_string(),
        frame_id,
        handle,
        width,
        height,
    });
    if dropped > 0 && dropped % 120 == 0 {
        debug!("[YOLO服务] 推理队列覆盖丢帧累计={}", dropped);
    }
    Ok(())
}

/// 清理会话级 YOLO 状态（会话停止/销毁时调用）。
///
/// 参数：
/// - `session_id`：会话 ID。
pub fn remove_session_state(session_id: &str) {
    if let Ok(mut rt) = YOLO_RUNTIME.lock() {
        rt.remove_session(session_id);
    }
}

/// 订阅会话级 YOLO 结果流。
///
/// 参数：
/// - `session_id`：订阅目标会话 ID；
/// - `sink`：FRB 结果流下沉通道。
///
/// 返回：
/// - `Ok(())`：订阅启动成功；
/// - `Err`：订阅启动失败。
pub async fn subscribe_yolo_results(
    session_id: String,
    sink: StreamSink<YoloFrameResult>,
) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(ScrcpyError::Other("session id is empty".to_string()));
    }

    debug!("[YOLO服务] 开始订阅结果：session_id={}", session_id);
    let mut rx = YOLO_RESULT_BUS.subscribe();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(v) => v,
            Err(e) => {
                warn!("[YOLO服务] 创建订阅运行时失败：err={}", e);
                return;
            }
        };

        rt.block_on(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if event.session_id != session_id {
                            continue;
                        }
                        if sink.add(event).is_err() {
                            debug!("[YOLO服务] 订阅端已关闭，结束转发");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("[YOLO服务] 结果流滞后，丢弃条数={}", n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("[YOLO服务] 结果总线关闭，结束订阅");
                        break;
                    }
                }
            }
        });
    });
    Ok(())
}

/// 发布 YOLO 单帧结果（供后续推理管道调用）。
///
/// 参数：
/// - `event`：单帧推理结果。
pub fn publish_yolo_result(event: YoloFrameResult) {
    let enabled = YOLO_RUNTIME
        .lock()
        .map(|rt| rt.is_session_enabled(&event.session_id))
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let _ = YOLO_RESULT_BUS.send(event);
}
