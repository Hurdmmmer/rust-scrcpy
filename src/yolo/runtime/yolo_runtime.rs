use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tracing::{debug, info, warn};

use crate::gh_common::model::{YoloConfig, YoloFrameResult};
use crate::gh_common::Result;

use crate::yolo::engine::yolo_engine::YoloEngine;
use crate::yolo::runtime::yolo_infer_pipeline::YoloInferPipeline;

/// YOLO 运行时（保存引擎与会话开关状态）。
#[derive(Debug, Default)]
pub struct YoloRuntime {
    engine: Option<YoloEngine>,
    enabled_sessions: HashSet<String>,
    infer_pipelines: HashMap<String, YoloInferPipeline>,
}

impl YoloRuntime {
    /// 初始化/更新运行时引擎。
    ///
    /// 参数：
    /// - `config`：最新 YOLO 配置。
    ///
    /// 返回：
    /// - `Ok(())`：引擎完成初始化；
    /// - `Err`：引擎初始化失败。
    pub fn init(&mut self, config: YoloConfig) -> Result<()> {
        info!("[YOLO运行时] 初始化引擎");
        let engine = YoloEngine::load(config)?;
        engine.warmup()?;
        let max_infer_fps = engine.config().max_infer_fps;
        for pipeline in self.infer_pipelines.values_mut() {
            pipeline.update_max_infer_fps(max_infer_fps);
        }
        self.engine = Some(engine);
        Ok(())
    }

    /// 运行中更新 YOLO 配置。
    ///
    /// 说明：
    /// - 当前阶段采用“重建引擎”策略完成热更新；
    /// - 会话开关状态会保留，不会因更新配置被清空。
    ///
    /// 参数：
    /// - `config`：新的 YOLO 推理配置。
    ///
    /// 返回：
    /// - `Ok(())`：配置更新成功；
    /// - `Err`：更新失败。
    pub fn update_config(&mut self, config: YoloConfig) -> Result<()> {
        info!("[YOLO运行时] 更新推理配置并重建引擎");
        let engine = YoloEngine::load(config)?;
        engine.warmup()?;
        let max_infer_fps = engine.config().max_infer_fps;
        for pipeline in self.infer_pipelines.values_mut() {
            pipeline.update_max_infer_fps(max_infer_fps);
        }
        self.engine = Some(engine);
        Ok(())
    }

    /// 设置会话开关状态。
    ///
    /// 参数：
    /// - `session_id`：会话 ID；
    /// - `enabled`：是否启用该会话的 YOLO 推理。
    pub fn set_session_enabled(&mut self, session_id: String, enabled: bool) {
        if enabled {
            debug!("[YOLO运行时] 启用会话推理：session_id={}", session_id);
            self.enabled_sessions.insert(session_id.clone());
            let fps = self
                .engine
                .as_ref()
                .map(|e| e.config().max_infer_fps)
                .unwrap_or(15);
            self.infer_pipelines
                .entry(session_id)
                .or_insert_with(|| YoloInferPipeline::new(fps));
        } else {
            debug!("[YOLO运行时] 禁用会话推理：session_id={}", session_id);
            self.enabled_sessions.remove(&session_id);
            self.infer_pipelines.remove(&session_id);
        }
    }

    /// 判断会话是否已启用 YOLO 推理。
    ///
    /// 参数：
    /// - `session_id`：会话 ID。
    pub fn is_session_enabled(&self, session_id: &str) -> bool {
        self.enabled_sessions.contains(session_id)
    }

    /// 移除会话运行时状态（会话销毁时调用）。
    ///
    /// 参数：
    /// - `session_id`：会话 ID。
    pub fn remove_session(&mut self, session_id: &str) {
        self.enabled_sessions.remove(session_id);
        self.infer_pipelines.remove(session_id);
    }

    /// 执行会话级 YOLO 推理（含限频）。
    ///
    /// 参数：
    /// - `session_id`：会话 ID；
    /// - `frame_id`：帧 ID；
    /// - `bgra`：BGRA 帧数据；
    /// - `width`：帧宽；
    /// - `height`：帧高；
    /// - `stride`：行步长。
    ///
    /// 返回：
    /// - `Ok(Some(result))`：本帧执行了推理并得到结果；
    /// - `Ok(None)`：本帧未推理（未启用/未初始化/限频跳过）；
    /// - `Err`：推理执行失败。
    pub fn infer_bgra_if_needed(
        &mut self,
        session_id: &str,
        frame_id: u64,
        bgra: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Option<YoloFrameResult>> {
        if !self.enabled_sessions.contains(session_id) {
            return Ok(None);
        }
        let Some(engine) = self.engine.as_ref() else {
            warn!(
                "[YOLO运行时] 跳过推理：引擎未初始化，session_id={}",
                session_id
            );
            return Ok(None);
        };

        let pipeline = self
            .infer_pipelines
            .entry(session_id.to_string())
            .or_insert_with(|| YoloInferPipeline::new(engine.config().max_infer_fps));
        if !pipeline.should_infer_now(Instant::now()) {
            return Ok(None);
        }

        let result = engine.infer_bgra(session_id, frame_id, bgra, width, height, stride)?;
        Ok(Some(result))
    }
}
