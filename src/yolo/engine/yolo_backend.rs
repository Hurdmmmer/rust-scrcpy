use crate::gh_common::model::YoloExecutionProvider;

/// YOLO 推理后端元信息（与上层配置分离，便于后续扩展后端专属参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YoloBackend {
    /// 当前后端类型。
    pub provider: YoloExecutionProvider,
}
