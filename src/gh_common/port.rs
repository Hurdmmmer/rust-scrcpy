use std::net::TcpListener;

use super::{Result, ScrcpyError};
use tracing::{debug, info};

/// 检查端口是否可用
pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// 从指定端口开始，寻找第一个可用端口
pub fn find_available_port(start_port: u16, max_attempts: u16) -> Result<u16> {
    let end_port = start_port.saturating_add(max_attempts);

    for port in start_port..=end_port {
        if is_port_available(port) {
            if port != start_port {
                info!("Port {} is occupied, using port {} instead", start_port, port);
            }
            return Ok(port);
        }
        debug!("Port {} is occupied, trying next...", port);
    }

    Err(ScrcpyError::NoAvailablePort(start_port, end_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_port_available() {
        let port = 59999;
        let _available = is_port_available(port);
    }

    #[test]
    fn test_find_available_port() {
        let result = find_available_port(50000, 100);
        assert!(result.is_ok());
        let port = result.expect("expected available port");
        assert!((50000..=50100).contains(&port));
    }
}
