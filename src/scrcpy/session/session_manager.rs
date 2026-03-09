use std::collections::HashMap;

use tracing::{debug, info, warn};

use crate::gh_common::{Result, ScrcpyError};
use crate::scrcpy::session::Session;

/// 会话管理器。
///
/// 职责边界：
/// - 只维护会话表（增删查改）；
/// - 可选维护会话元信息缓存（后续补充）；
/// - 不接收配置、不负责建链、不承担事件分发。
#[derive(Default)]
pub struct SessionManager {
    /// 会话表：键为会话 ID，值为已连接成功的会话对象。
    sessions: HashMap<String, Session>,
}

impl SessionManager {
    /// 创建空会话管理器。
    pub fn new() -> Self {
        info!("[会话管理器] 初始化完成");
        Self {
            sessions: HashMap::new(),
        }
    }

    /// 插入会话。
    ///
    /// 语义：
    /// - `session_id` 已存在时返回错误，避免无意覆盖；
    /// - 只接受“已连接成功”的 `Session` 对象。
    pub fn insert(&mut self, session_id: String, session: Session) -> Result<()> {
        debug!("[会话管理器] 尝试插入会话: session_id={}", session_id);
        if self.sessions.contains_key(&session_id) {
            warn!("[会话管理器] 插入失败，会话已存在: session_id={}", session_id);
            return Err(ScrcpyError::Other(format!(
                "session already exists: {}",
                session_id
            )));
        }

        self.sessions.insert(session_id.clone(), session);
        info!(
            "[会话管理器] 插入会话成功: session_id={}, 当前会话数={}",
            session_id,
            self.sessions.len()
        );
        Ok(())
    }

    /// 获取会话只读引用。
    pub fn get(&self, session_id: &str) -> Option<&Session> {
        debug!("[会话管理器] 读取会话: session_id={}", session_id);
        self.sessions.get(session_id)
    }

    /// 获取会话可变引用。
    pub fn get_mut(&mut self, session_id: &str) -> Option<&mut Session> {
        debug!("[会话管理器] 获取可变会话: session_id={}", session_id);
        self.sessions.get_mut(session_id)
    }

    /// 移除会话。
    pub fn remove(&mut self, session_id: &str) -> Option<Session> {
        debug!("[会话管理器] 移除会话: session_id={}", session_id);
        let removed = self.sessions.remove(session_id);
        if removed.is_some() {
            info!(
                "[会话管理器] 移除会话成功: session_id={}, 当前会话数={}",
                session_id,
                self.sessions.len()
            );
        } else {
            warn!("[会话管理器] 移除会话失败，会话不存在: session_id={}", session_id);
        }
        removed
    }

    /// 返回当前会话总数。
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// 返回会话表是否为空。
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// 列出全部会话 ID。
    pub fn list_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sessions.keys().cloned().collect();
        ids.sort();
        debug!("[会话管理器] 列出会话 ID: count={}", ids.len());
        ids
    }

    /// 清空全部会话。
    ///
    /// 注意：
    /// - 本方法只清理会话表；
    /// - 会话停止与资源释放由上层先行保证。
    pub fn clear(&mut self) {
        let before = self.sessions.len();
        self.sessions.clear();
        info!("[会话管理器] 清空会话表: 清理前={}, 清理后=0", before);
    }
}
