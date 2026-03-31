use std::time::{Duration, Instant};

/// YOLO 推理限频管道（会话级）。
///
/// 作用：
/// - 按 `max_infer_fps` 控制推理频率，避免推理拖垮解码与渲染主链路；
/// - 记录限频丢帧统计，便于后续调优。
#[derive(Debug, Clone)]
pub struct YoloInferPipeline {
    /// 目标推理最小间隔。
    min_interval: Duration,
    /// 最近一次实际执行推理的时间点。
    last_infer_at: Option<Instant>,
    /// 总输入帧计数（进入限频判断的帧数）。
    total_frames: u64,
    /// 因限频被跳过的帧计数。
    skipped_by_rate_limit: u64,
}

impl YoloInferPipeline {
    /// 创建限频管道。
    ///
    /// 参数：
    /// - `max_infer_fps`：每秒最多推理帧数（必须 > 0）。
    pub fn new(max_infer_fps: u32) -> Self {
        let fps = max_infer_fps.max(1) as u64;
        let interval_ms = (1000u64 / fps).max(1);
        Self {
            min_interval: Duration::from_millis(interval_ms),
            last_infer_at: None,
            total_frames: 0,
            skipped_by_rate_limit: 0,
        }
    }

    /// 更新限频参数（配置热更新时调用）。
    ///
    /// 参数：
    /// - `max_infer_fps`：新的推理上限 FPS。
    pub fn update_max_infer_fps(&mut self, max_infer_fps: u32) {
        let fps = max_infer_fps.max(1) as u64;
        let interval_ms = (1000u64 / fps).max(1);
        self.min_interval = Duration::from_millis(interval_ms);
    }

    /// 判断当前帧是否允许执行推理。
    ///
    /// 返回：
    /// - `true`：允许执行推理；
    /// - `false`：本帧应跳过（限频触发）。
    pub fn should_infer_now(&mut self, now: Instant) -> bool {
        self.total_frames = self.total_frames.saturating_add(1);
        match self.last_infer_at {
            None => {
                self.last_infer_at = Some(now);
                true
            }
            Some(last) => {
                if now.duration_since(last) >= self.min_interval {
                    self.last_infer_at = Some(now);
                    true
                } else {
                    self.skipped_by_rate_limit = self.skipped_by_rate_limit.saturating_add(1);
                    false
                }
            }
        }
    }

    /// 返回限频跳过次数。
    pub fn skipped_by_rate_limit(&self) -> u64 {
        self.skipped_by_rate_limit
    }
}

impl Default for YoloInferPipeline {
    fn default() -> Self {
        Self::new(15)
    }
}
