// 当前 DLL 主链路不使用该旧设备模型，先整体注释保留，避免误删历史实现。
//
// use serde::{Deserialize, Serialize};
//
// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct Device {
//     pub id: String,
//     pub model: Option<String>,
//     pub android_version: Option<String>,
//     pub screen_size: Option<(u32, u32)>,
// }
//
// impl Device {
//     pub fn new(id: String) -> Self {
//         Self {
//             id,
//             model: None,
//             android_version: None,
//             screen_size: None,
//         }
//     }
//
//     pub fn with_info(
//         id: String,
//         model: String,
//         android_version: String,
//         screen_size: (u32, u32),
//     ) -> Self {
//         Self {
//             id,
//             model: Some(model),
//             android_version: Some(android_version),
//             screen_size: Some(screen_size),
//         }
//     }
// }
