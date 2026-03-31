use crate::gh_common::model::{YoloConfig, YoloExecutionProvider};
use crate::gh_common::{Result, ScrcpyError};

/// 校验 YOLO 配置有效性（仅允许硬件推理后端）。
///
/// 参数：
/// - `config`：待校验的 YOLO 配置。
///
/// 返回：
/// - `Ok(())`：配置合法，可用于初始化推理引擎；
/// - `Err`：配置非法，错误文案可直接透传上层显示。
pub fn validate_yolo_config(config: &YoloConfig) -> Result<()> {
    if config.model_path.trim().is_empty() {
        return Err(ScrcpyError::Other("yolo model path is empty".to_string()));
    }
    if config.input_width == 0 || config.input_height == 0 {
        return Err(ScrcpyError::Other("yolo input size is invalid".to_string()));
    }
    if !(0.0..=1.0).contains(&config.confidence_threshold) {
        return Err(ScrcpyError::Other(
            "confidence threshold out of range".to_string(),
        ));
    }
    if !(0.0..=1.0).contains(&config.iou_threshold) {
        return Err(ScrcpyError::Other("iou threshold out of range".to_string()));
    }
    if config.max_detections == 0 {
        return Err(ScrcpyError::Other("max detections must be > 0".to_string()));
    }
    if config.max_infer_fps == 0 {
        return Err(ScrcpyError::Other("max infer fps must be > 0".to_string()));
    }

    // 当前阶段仅支持硬件推理后端。
    match config.provider {
        YoloExecutionProvider::DirectMl
        | YoloExecutionProvider::Cuda
        | YoloExecutionProvider::TensorRt => Ok(()),
    }
}
