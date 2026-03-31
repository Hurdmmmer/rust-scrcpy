#![allow(unexpected_cfgs)]
#[cfg(feature = "frb")]
mod frb_generated;

// Windows Runner 原生回调注册模块（不进入 FRB API 扫描）。
pub(crate) mod flutter_callback_register;

pub mod gh_api;
pub mod gh_common;
pub mod scrcpy;
pub mod yolo;
