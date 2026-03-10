use serde::{Deserialize, Serialize};

/// 输入后端模式。
///
/// 语义说明：
/// - Inject：使用 scrcpy 现有注入协议（当前稳定链路）；
/// - Uhid：使用 UHID 虚拟外设协议（后续逐步补齐完整实现）；
/// - Auto：优先尝试 UHID，失败后回退 Inject（生产推荐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrcpyInputMode {
    Inject,
    Uhid,
    Auto,
}

impl Default for ScrcpyInputMode {
    fn default() -> Self {
        // 默认走自动模式，便于后续逐步打开 UHID 能力。
        ScrcpyInputMode::Auto
    }
}
