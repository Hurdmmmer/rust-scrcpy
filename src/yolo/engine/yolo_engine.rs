use std::sync::Mutex;
use std::time::Instant;

use ndarray::{Array4, ArrayViewD};
use ort::execution_providers::{
    CUDAExecutionProvider, DirectMLExecutionProvider, ExecutionProviderDispatch,
    TensorRTExecutionProvider,
};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use tracing::{debug, info};

use crate::gh_common::model::{YoloConfig, YoloDetection, YoloExecutionProvider, YoloFrameResult};
use crate::gh_common::{Result, ScrcpyError};
use crate::yolo::model::yolo_result::build_empty_result;

use super::yolo_backend::YoloBackend;

/// YOLO 推理引擎（ONNX Runtime + 硬件后端）。
///
/// 说明：
/// - 会话对象需要可变访问（`run` 需要 `&mut Session`），因此使用 `Mutex` 保护；
/// - 当前实现优先支持常见 YOLOv8 ONNX 输出格式，并带基础 NMS；
/// - 严格按配置要求走硬件后端，不主动回退 CPU 推理。
pub struct YoloEngine {
    /// 当前加载的配置。
    config: YoloConfig,
    /// 当前后端信息。
    backend: YoloBackend,
    /// ONNX Runtime 会话（执行推理）。
    session: Mutex<Session>,
}

impl std::fmt::Debug for YoloEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YoloEngine")
            .field("config", &self.config)
            .field("backend", &self.backend)
            .finish()
    }
}

impl YoloEngine {
    /// 返回当前引擎配置快照。
    pub fn config(&self) -> &YoloConfig {
        &self.config
    }

    /// 创建并加载 YOLO 引擎。
    ///
    /// 参数：
    /// - `config`：推理配置（模型路径、输入尺寸、阈值与后端）。
    ///
    /// 返回：
    /// - `Ok(Self)`：引擎初始化成功；
    /// - `Err`：引擎初始化失败。
    pub fn load(config: YoloConfig) -> Result<Self> {
        info!(
            "[YOLO引擎] 开始加载：model_path={}, provider={:?}, input={}x{}",
            config.model_path, config.provider, config.input_width, config.input_height
        );
        let backend = YoloBackend {
            provider: config.provider,
        };
        let session = Self::build_session(&config)?;
        info!("[YOLO引擎] 模型加载完成");
        Ok(Self {
            config,
            backend,
            session: Mutex::new(session),
        })
    }

    /// 执行一次 warmup（真实推理预热）。
    ///
    /// 参数：
    /// - 无。
    ///
    /// 返回：
    /// - `Ok(())`：warmup 成功；
    /// - `Err`：warmup 失败。
    pub fn warmup(&self) -> Result<()> {
        let start = Instant::now();
        let input = Array4::<f32>::zeros((
            1,
            3,
            self.config.input_height as usize,
            self.config.input_width as usize,
        ));
        let input_tensor = TensorRef::from_array_view(input.view())
            .map_err(|e| ScrcpyError::Other(format!("yolo warmup input tensor failed: {e}")))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| ScrcpyError::Other("yolo session lock poisoned".to_string()))?;
        let _outputs = session
            .run(ort::inputs![input_tensor])
            .map_err(|e| ScrcpyError::Other(format!("yolo warmup run failed: {e}")))?;
        debug!(
            "[YOLO引擎] warmup完成：provider={:?}, model_path={}, cost={}ms",
            self.backend.provider,
            self.config.model_path,
            start.elapsed().as_millis()
        );
        Ok(())
    }

    /// 对 BGRA 帧执行一次 YOLO 推理。
    ///
    /// 参数：
    /// - `_session_id`：会话 ID；
    /// - `_frame_id`：帧 ID；
    /// - `_bgra`：BGRA 原始像素；
    /// - `_width`：帧宽度；
    /// - `_height`：帧高度；
    /// - `_stride`：行步长字节数。
    ///
    /// 返回：
    /// - `Ok(YoloFrameResult)`：推理完成（可能零目标）；
    /// - `Err`：推理失败。
    pub fn infer_bgra(
        &self,
        session_id: &str,
        frame_id: u64,
        bgra: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<YoloFrameResult> {
        let start = Instant::now();
        if width == 0 || height == 0 {
            return Err(ScrcpyError::Other("invalid frame size".to_string()));
        }

        let input = Self::preprocess_bgra_to_nchw(
            bgra,
            width,
            height,
            stride,
            self.config.input_width,
            self.config.input_height,
        )?;
        let input_tensor = TensorRef::from_array_view(input.view())
            .map_err(|e| ScrcpyError::Other(format!("yolo input tensor failed: {e}")))?;

        let mut session = self
            .session
            .lock()
            .map_err(|_| ScrcpyError::Other("yolo session lock poisoned".to_string()))?;
        let outputs = session
            .run(ort::inputs![input_tensor])
            .map_err(|e| ScrcpyError::Other(format!("yolo run failed: {e}")))?;

        if outputs.len() == 0 {
            return Err(ScrcpyError::Other("yolo output is empty".to_string()));
        }
        let output0 = &outputs[0];
        let output = output0
            .try_extract_array::<f32>()
            .map_err(|e| ScrcpyError::Other(format!("yolo extract output failed: {e}")))?;

        let mut detections = Self::parse_yolo_output(
            output,
            &self.config,
            width as f32,
            height as f32,
            self.config.input_width as f32,
            self.config.input_height as f32,
        )?;
        detections = Self::apply_nms(detections, self.config.iou_threshold);
        if detections.len() > self.config.max_detections as usize {
            detections.truncate(self.config.max_detections as usize);
        }

        let latency_ms = start.elapsed().as_millis() as u32;
        debug!(
            "[YOLO引擎] 推理完成：session_id={}, frame_id={}, det_count={}, latency={}ms",
            session_id,
            frame_id,
            detections.len(),
            latency_ms
        );

        if detections.is_empty() {
            return Ok(build_empty_result(
                session_id.to_string(),
                frame_id,
                width,
                height,
                latency_ms,
            ));
        }

        Ok(YoloFrameResult {
            session_id: session_id.to_string(),
            frame_id,
            frame_width: width,
            frame_height: height,
            infer_latency_ms: latency_ms,
            detections,
        })
    }

    /// 构建 ONNX Runtime 会话。
    ///
    /// 参数：
    /// - `config`：YOLO 配置。
    ///
    /// 返回：
    /// - `Ok(Session)`：会话创建成功；
    /// - `Err`：会话创建失败。
    fn build_session(config: &YoloConfig) -> Result<Session> {
        let ep = Self::build_execution_provider(config.provider, config.device_index)?;
        let session = Session::builder()
            .map_err(|e| ScrcpyError::Other(format!("create yolo session builder failed: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| ScrcpyError::Other(format!("set yolo graph opt failed: {e}")))?
            .with_intra_threads(1)
            .map_err(|e| ScrcpyError::Other(format!("set yolo intra threads failed: {e}")))?
            .with_execution_providers([ep])
            .map_err(|e| ScrcpyError::Other(format!("set yolo execution provider failed: {e}")))?
            .commit_from_file(&config.model_path)
            .map_err(|e| ScrcpyError::Other(format!("load yolo model failed: {e}")))?;
        Ok(session)
    }

    /// 根据配置构建执行后端。
    ///
    /// 参数：
    /// - `provider`：后端类型；
    /// - `device_index`：可选设备索引（当前仅记录日志，后续按 EP 深化）。
    fn build_execution_provider(
        provider: YoloExecutionProvider,
        device_index: Option<u32>,
    ) -> Result<ExecutionProviderDispatch> {
        if let Some(idx) = device_index {
            debug!(
                "[YOLO引擎] 收到 device_index={}（当前版本暂未透传到 EP）",
                idx
            );
        }
        let ep = match provider {
            YoloExecutionProvider::DirectMl => DirectMLExecutionProvider::default()
                .build()
                .error_on_failure(),
            YoloExecutionProvider::Cuda => {
                CUDAExecutionProvider::default().build().error_on_failure()
            }
            YoloExecutionProvider::TensorRt => TensorRTExecutionProvider::default()
                .build()
                .error_on_failure(),
        };
        Ok(ep)
    }

    /// BGRA 图像预处理为 NCHW Float32 张量。
    ///
    /// 参数：
    /// - `bgra`：原始 BGRA 数据；
    /// - `src_w`/`src_h`：源图尺寸；
    /// - `src_stride`：源图行步长；
    /// - `dst_w`/`dst_h`：目标输入尺寸。
    fn preprocess_bgra_to_nchw(
        bgra: &[u8],
        src_w: u32,
        src_h: u32,
        src_stride: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Array4<f32>> {
        let required = (src_stride as usize) * (src_h as usize);
        if bgra.len() < required {
            return Err(ScrcpyError::Other(format!(
                "bgra buffer too small: got={}, required={}",
                bgra.len(),
                required
            )));
        }

        let mut input = Array4::<f32>::zeros((1, 3, dst_h as usize, dst_w as usize));
        let x_scale = src_w as f32 / dst_w as f32;
        let y_scale = src_h as f32 / dst_h as f32;

        for dy in 0..dst_h as usize {
            let sy = ((dy as f32) * y_scale).floor() as usize;
            for dx in 0..dst_w as usize {
                let sx = ((dx as f32) * x_scale).floor() as usize;
                let src_offset = sy * (src_stride as usize) + sx * 4;
                if src_offset + 2 >= bgra.len() {
                    continue;
                }
                let b = bgra[src_offset] as f32 / 255.0;
                let g = bgra[src_offset + 1] as f32 / 255.0;
                let r = bgra[src_offset + 2] as f32 / 255.0;
                input[[0, 0, dy, dx]] = r;
                input[[0, 1, dy, dx]] = g;
                input[[0, 2, dy, dx]] = b;
            }
        }
        Ok(input)
    }

    /// 解析 YOLO 输出张量（兼容常见 2D/3D 输出布局）。
    ///
    /// 参数：
    /// - `output`：模型输出；
    /// - `config`：推理配置；
    /// - `frame_w`/`frame_h`：原图尺寸；
    /// - `input_w`/`input_h`：模型输入尺寸。
    fn parse_yolo_output(
        output: ArrayViewD<'_, f32>,
        config: &YoloConfig,
        frame_w: f32,
        frame_h: f32,
        input_w: f32,
        input_h: f32,
    ) -> Result<Vec<YoloDetection>> {
        let shape = output.shape().to_vec();
        debug!("[YOLO引擎] 输出维度: {:?}", shape);

        let mut rows: Vec<Vec<f32>> = Vec::new();
        match shape.as_slice() {
            // [N, A]
            [n, a] => {
                for i in 0..*n {
                    let mut row = Vec::with_capacity(*a);
                    for j in 0..*a {
                        row.push(output[[i, j]]);
                    }
                    rows.push(row);
                }
            }
            // [1, N, A] 或 [1, A, N]
            [1, d1, d2] => {
                if d2 >= d1 {
                    // 视作 [1, N, A]
                    for i in 0..*d1 {
                        let mut row = Vec::with_capacity(*d2);
                        for j in 0..*d2 {
                            row.push(output[[0, i, j]]);
                        }
                        rows.push(row);
                    }
                } else {
                    // 视作 [1, A, N]，转置成行
                    for i in 0..*d2 {
                        let mut row = Vec::with_capacity(*d1);
                        for j in 0..*d1 {
                            row.push(output[[0, j, i]]);
                        }
                        rows.push(row);
                    }
                }
            }
            _ => {
                return Err(ScrcpyError::Other(format!(
                    "unsupported yolo output shape: {:?}",
                    shape
                )));
            }
        }

        let mut out = Vec::<YoloDetection>::new();
        let sx = frame_w / input_w;
        let sy = frame_h / input_h;
        for row in rows {
            if row.len() < 6 {
                continue;
            }

            let cx = row[0];
            let cy = row[1];
            let w = row[2];
            let h = row[3];

            let (score, class_id) = if row.len() == 6 {
                (row[4], row[5].max(0.0) as u32)
            } else {
                // YOLOv8 常见格式：4 + num_classes（无独立 objectness）。
                let mut best_idx = 0usize;
                let mut best_score = 0.0f32;
                for (i, s) in row[4..].iter().enumerate() {
                    if *s > best_score {
                        best_score = *s;
                        best_idx = i;
                    }
                }
                (best_score, best_idx as u32)
            };

            if score < config.confidence_threshold {
                continue;
            }

            let x = (cx - w * 0.5) * sx;
            let y = (cy - h * 0.5) * sy;
            let ww = w * sx;
            let hh = h * sy;

            if ww <= 1.0 || hh <= 1.0 {
                continue;
            }

            out.push(YoloDetection {
                class_id,
                label: None,
                score,
                x: x.clamp(0.0, frame_w - 1.0),
                y: y.clamp(0.0, frame_h - 1.0),
                width: ww.clamp(0.0, frame_w),
                height: hh.clamp(0.0, frame_h),
            });
        }
        Ok(out)
    }

    /// 对检测框执行 NMS，降低重复框。
    fn apply_nms(mut detections: Vec<YoloDetection>, iou_threshold: f32) -> Vec<YoloDetection> {
        if detections.is_empty() {
            return detections;
        }
        detections.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut kept = Vec::<YoloDetection>::new();
        'outer: for det in detections {
            for k in &kept {
                if det.class_id != k.class_id {
                    continue;
                }
                let iou = Self::bbox_iou(&det, k);
                if iou >= iou_threshold {
                    continue 'outer;
                }
            }
            kept.push(det);
        }
        kept
    }

    /// 计算两个检测框 IoU。
    fn bbox_iou(a: &YoloDetection, b: &YoloDetection) -> f32 {
        let ax2 = a.x + a.width;
        let ay2 = a.y + a.height;
        let bx2 = b.x + b.width;
        let by2 = b.y + b.height;

        let inter_x1 = a.x.max(b.x);
        let inter_y1 = a.y.max(b.y);
        let inter_x2 = ax2.min(bx2);
        let inter_y2 = ay2.min(by2);

        let inter_w = (inter_x2 - inter_x1).max(0.0);
        let inter_h = (inter_y2 - inter_y1).max(0.0);
        let inter_area = inter_w * inter_h;
        if inter_area <= 0.0 {
            return 0.0;
        }

        let area_a = (a.width.max(0.0)) * (a.height.max(0.0));
        let area_b = (b.width.max(0.0)) * (b.height.max(0.0));
        let union = area_a + area_b - inter_area;
        if union <= 0.0 {
            0.0
        } else {
            inter_area / union
        }
    }
}
