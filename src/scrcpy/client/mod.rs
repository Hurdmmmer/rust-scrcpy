//! Scrcpy Client 模块。
//!
//! 迁移阶段说明：
//! - `ScrcpyClient` 作为上层入口对象；
//! - `ScrcpyConn` 提供底层建链原语；
//! - `Session` 承接单会话资源；
//! - 控制通道与视频读取器作为 client 子能力模块存在。

pub mod scrcpy_client;
pub mod scrcpy_conn;
pub mod scrcpy_control;
pub mod scrcpy_video_stream;

pub use scrcpy_client::ScrcpyClient;
pub use scrcpy_conn::ScrcpyConnect;
