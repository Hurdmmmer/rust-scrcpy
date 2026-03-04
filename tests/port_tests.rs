use rust_scrcpy::utils::port::{find_available_port, is_port_available};

#[test]
fn port_availability_check_runs() {
    // 选高位端口做可用性探测（结果允许 true/false）。
    let _ = is_port_available(59999);
}

#[test]
fn find_available_port_returns_port_in_range() {
    let start = 50000;
    let max_attempts = 100;
    let result = find_available_port(start, max_attempts).expect("should find a free port");
    assert!(result >= start && result <= start + max_attempts);
}
