/// Flutter 对外 API 分层说明：
/// - `flutter_api`：暴露给 FRB 生成，供 Dart 侧调用。
/// - 原生回调注册已迁移到 crate 根模块 `flutter_callback_register`，不再放在 gh_api 内。
///
/// 这样做的原因：
/// - 保留 FRB 对 `gh_api` 的完整业务扫描（模型/服务不丢失）；
/// - 同时避免 callback 中 `*mut c_void` 被 FRB 扫描进入桥接层。
pub mod flutter_api;

pub use flutter_api::*;
