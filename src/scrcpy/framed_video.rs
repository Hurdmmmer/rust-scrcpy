use crate::error::{Result, ScrcpyError};
use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tracing::debug;

/// scrcpy 分帧协议常量（与 scrcpy `demuxer.c` 一致）。
///
/// 说明：
/// - `SC_PACKET_HEADER_SIZE = 12`：每个包头固定 12 字节；
/// - 前 8 字节是 `pts_flags`（大端 u64）；
/// - 后 4 字节是 `packet_size`（大端 u32）；
/// - `pts_flags` 的最高两位是标志位，低 62 位才是 PTS。
const SC_PACKET_HEADER_SIZE: usize = 12;
const SC_PACKET_FLAG_CONFIG: u64 = 1u64 << 63;
const SC_PACKET_FLAG_KEY_FRAME: u64 = 1u64 << 62;
const SC_PACKET_PTS_MASK: u64 = SC_PACKET_FLAG_KEY_FRAME - 1;
const PACKET_LOG_INTERVAL: u64 = 120;

/// scrcpy 码流中的 codec_id（4 字节 ASCII 大端）。
///
/// 协议来源：
/// - scrcpy 在启用 `send_codec_meta=true` 且 `raw_stream=false` 时，
///   会先发送 codec_id，再发送视频尺寸，然后进入 packet 循环。
// 当前主链路不读取 codec_meta，先注释保留。
// pub const SC_CODEC_ID_H264: u32 = 0x6832_3634; // "h264"
// pub const SC_CODEC_ID_H265: u32 = 0x6832_3635; // "h265"
// pub const SC_CODEC_ID_AV1: u32 = 0x0061_7631; // "av1"

/// 分帧模式下的协议头信息。
// #[derive(Debug, Clone)]
// pub struct FramedCodecMeta {
//     /// 编码器 ID（如 "h264"）。
//     pub codec_id: u32,
//     /// 视频宽度（像素）。
//     pub width: u32,
//     /// 视频高度（像素）。
//     pub height: u32,
// }

/// 一个完整的 scrcpy 分帧数据包。
///
/// 字段解释（与 scrcpy C 客户端语义对齐）：
/// - `is_config=true`：配置包（例如 H264 的 SPS/PPS），该包不参与正常 PTS 时间轴；
/// - `is_keyframe=true`：关键帧（IDR）；
/// - `data`：原始编码包数据（不是解码后的像素）。
#[derive(Debug, Clone)]
pub struct FramedVideoPacket {
    pub is_config: bool,
    pub is_keyframe: bool,
    pub data: Bytes,
}

/// scrcpy 分帧协议读取器（raw_stream=false 路径）。
///
/// 设计目标：
/// - 严格按 scrcpy 包格式读取，避免把 NAL 边界判断放在 TCP 分片层；
/// - 让上层解码器直接消费“完整编码包”，降低花屏/错帧概率；
/// - 与旧 `VideoStreamReader` 并行存在，便于灰度切换和回归。
pub struct FramedVideoStreamReader {
    stream: TcpStream,
    packet_count: u64,
}

impl FramedVideoStreamReader {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            packet_count: 0,
        }
    }

    // pub fn packet_count(&self) -> u64 {
    //     self.packet_count
    // }

    // /// 读取 codec/meta 头（12 字节：codec_id + width + height）。
    // ///
    // /// 注意：
    // /// - 这个函数只用于 `send_codec_meta=true` 的模式；
    // /// - 若设备端关闭该选项，调用方应走兼容分支而不是硬读本头。
    // pub async fn read_codec_meta(&mut self) -> Result<FramedCodecMeta> {
    //     let mut buf = [0u8; 12];
    //     self.stream
    //         .read_exact(&mut buf)
    //         .await
    //         .map_err(|e| ScrcpyError::VideoStream(format!("read codec meta failed: {}", e)))?;
    //
    //     let codec_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    //     let width = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    //     let height = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    //
    //     Ok(FramedCodecMeta {
    //         codec_id,
    //         width,
    //         height,
    //     })
    // }

    /// 读取下一个分帧包。
    ///
    /// 返回：
    /// - `Ok(Some(packet))`：成功读取一个包；
    /// - `Ok(None)`：EOF（设备断开/流结束）；
    /// - `Err(...)`： 协议错误或网络错误。
    pub async fn read_packet(&mut self) -> Result<Option<FramedVideoPacket>> {
        let mut header = [0u8; SC_PACKET_HEADER_SIZE];
        match self.stream.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => {
                return Err(ScrcpyError::VideoStream(format!(
                    "read packet header failed: {}",
                    e
                )))
            }
        }

        let pts_flags = u64::from_be_bytes([
            header[0], header[1], header[2], header[3], header[4], header[5], header[6], header[7],
        ]);
        let packet_size = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;

        if packet_size == 0 {
            return Err(ScrcpyError::VideoStream(
                "invalid framed packet: size=0".to_string(),
            ));
        }

        let mut payload = vec![0u8; packet_size];
        match self.stream.read_exact(&mut payload).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => {
                return Err(ScrcpyError::VideoStream(format!(
                    "read packet payload failed: {}",
                    e
                )))
            }
        }

        let is_config = (pts_flags & SC_PACKET_FLAG_CONFIG) != 0;
        let is_keyframe = (pts_flags & SC_PACKET_FLAG_KEY_FRAME) != 0;
        let pts_us = if is_config { None } else { Some(pts_flags & SC_PACKET_PTS_MASK) };

        self.packet_count += 1;
        if self.packet_count % PACKET_LOG_INTERVAL == 0 || is_config || is_keyframe {
            debug!(
                "framed packet: seq={}, size={}, config={}, key={}, pts_us={:?}",
                self.packet_count, packet_size, is_config, is_keyframe, pts_us
            );
        }

        Ok(Some(FramedVideoPacket {
            is_config,
            is_keyframe,
            data: Bytes::from(payload),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pts_flags_works() {
        let pts: u64 = 123456;
        let pts_flags = SC_PACKET_FLAG_KEY_FRAME | pts;
        let is_config = (pts_flags & SC_PACKET_FLAG_CONFIG) != 0;
        let is_keyframe = (pts_flags & SC_PACKET_FLAG_KEY_FRAME) != 0;
        let parsed_pts = pts_flags & SC_PACKET_PTS_MASK;

        assert!(!is_config);
        assert!(is_keyframe);
        assert_eq!(parsed_pts, pts);
    }
}
