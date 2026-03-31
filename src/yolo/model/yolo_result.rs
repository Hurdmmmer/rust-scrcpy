use crate::gh_common::model::{YoloDetection, YoloFrameResult};

/// 构造空检测结果（无目标命中场景）。
///
/// 参数：
/// - `session_id`：会话 ID；
/// - `frame_id`：帧 ID；
/// - `frame_width`：原始帧宽；
/// - `frame_height`：原始帧高；
/// - `infer_latency_ms`：推理耗时（毫秒）。
pub fn build_empty_result(
    session_id: String,
    frame_id: u64,
    frame_width: u32,
    frame_height: u32,
    infer_latency_ms: u32,
) -> YoloFrameResult {
    YoloFrameResult {
        session_id,
        frame_id,
        frame_width,
        frame_height,
        infer_latency_ms,
        detections: Vec::<YoloDetection>::new(),
    }
}
