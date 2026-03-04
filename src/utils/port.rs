//! 端口工具模块 - 提供端口可用性检测和自动寻找功能

use std::net::TcpListener;
use crate::error::{Result, ScrcpyError};
use tracing::{debug, info};

/// 检查端口是否可用
pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// 从指定端口开始，寻找第一个可用端口
///
/// # Arguments
/// * `start_port` - 起始端口
/// * `max_attempts` - 最大尝试次数（向后搜索的范围）
///
/// # Returns
/// * `Ok(port)` - 找到的可用端口
/// * `Err` - 在范围内未找到可用端口
pub fn find_available_port(start_port: u16, max_attempts: u16) -> Result<u16> {
    let end_port = start_port.saturating_add(max_attempts);

    for port in start_port..=end_port {
        if is_port_available(port) {
            if port != start_port {
                info!("📌 Port {} is occupied, using port {} instead", start_port, port);
            }
            return Ok(port);
        }
        debug!("Port {} is occupied, trying next...", port);
    }

    Err(ScrcpyError::NoAvailablePort(start_port, end_port))
}

// 目前 DLL 主链路未使用“批量找端口”，先注释保留。
// /// 寻找多个连续可用端口
// ///
// /// # Arguments
// /// * `start_port` - 起始端口
// /// * `count` - 需要的端口数量
// /// * `max_attempts` - 每个端口的最大尝试次数
// ///
// /// # Returns
// /// * `Ok(Vec<u16>)` - 找到的可用端口列表
// /// * `Err` - 未能找到足够的可用端口
// pub fn find_available_ports(
//     start_port: u16,
//     count: usize,
//     max_attempts: u16,
// ) -> Result<Vec<u16>> {
//     let mut ports = Vec::with_capacity(count);
//     let mut current_port = start_port;
//
//     for i in 0..count {
//         let port = find_available_port(current_port, max_attempts)?;
//         ports.push(port);
//         // 下一个端口从当前端口+1开始搜索，避免冲突
//         current_port = port.saturating_add(1);
//
//         debug!("Found available port {} for slot {}", port, i);
//     }
//
//     Ok(ports)
// }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_port_available() {
        // 测试一个通常可用的高端口
        let port = 59999;
        let available = is_port_available(port);
        println!("Port {} available: {}", port, available);
    }

    #[test]
    fn test_find_available_port() {
        let result = find_available_port(50000, 100);
        assert!(result.is_ok());
        let port = result.unwrap();
        assert!(port >= 50000 && port <= 50100);
        println!("Found available port: {}", port);
    }
}
