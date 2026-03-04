mod frb_generated; 

mod adb;
mod config;
mod scrcpy;
mod session;
pub mod utils;

pub mod error;
pub mod decoder;
pub mod rust_scrcpy_api;
#[path = "rust_scrcpy_api/flutter_callback_register.rs"]
mod flutter_callback_register;

/// 统一导出 Result 与错误类型，便于跨模块复用。
pub use error::{Result, ScrcpyError};

const SC_PACKET_FLAG_CONFIG: u64 = 1u64 << 63;
const SC_PACKET_FLAG_KEY_FRAME: u64 = 1u64 << 62;
const SC_PACKET_PTS_MASK: u64 = SC_PACKET_FLAG_KEY_FRAME - 1;

/// 解析 scrcpy framed 头中的 `pts_flags`。
pub fn decode_pts_flags(pts_flags: u64) -> (bool, bool, Option<u64>) {
    let is_config = (pts_flags & SC_PACKET_FLAG_CONFIG) != 0;
    let is_keyframe = (pts_flags & SC_PACKET_FLAG_KEY_FRAME) != 0;
    let pts = if is_config {
        None
    } else {
        Some(pts_flags & SC_PACKET_PTS_MASK)
    };
    (is_config, is_keyframe, pts)
}

fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= buf.len() {
        if i + 4 <= buf.len()
            && buf[i] == 0
            && buf[i + 1] == 0
            && buf[i + 2] == 0
            && buf[i + 3] == 1
        {
            return Some((i, 4));
        }
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

/// 从 Annex-B 缓冲中提取一个 NAL（不含 start code）。
///
/// 返回 `(nal, consumed)`：
/// - 若前缀有噪声字节，先返回空 NAL 并推进到首个 start code 前；
/// - 若找到 NAL，`consumed` 会停在下一段 start code 起点（或缓冲尾）。
pub fn extract_annexb_nal(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let (start_pos, start_len) = find_start_code(buf, 0)?;
    if start_pos > 0 {
        return Some((Vec::<u8>::new(), start_pos));
    }

    let nal_start = start_pos + start_len;
    if nal_start >= buf.len() {
        return Some((Vec::new(), nal_start));
    }

    if let Some((next_start_pos, _)) = find_start_code(buf, nal_start) {
        return Some((buf[nal_start..next_start_pos].to_vec(), next_start_pos));
    }

    Some((buf[nal_start..].to_vec(), buf.len()))
}

