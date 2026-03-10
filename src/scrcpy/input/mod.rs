//! 输入客户端抽象层。
//!
//! 设计目标：
//! - 让 Session 只依赖统一输入接口，不感知 Inject/UHID 实现差异；
//! - 在不破坏现有链路的前提下逐步引入 UHID 键盘能力。

pub mod input_mode;
pub mod input_client;
pub mod inject_input_client;
pub mod uhid_input_client;
pub mod uhid_keyboard_state;

pub use input_mode::ScrcpyInputMode;
pub use input_client::ScrcpyInputClient;
pub use inject_input_client::InjectInputClient;
pub use uhid_input_client::UhidInputClient;
pub use uhid_keyboard_state::UhidKeyboardState;

