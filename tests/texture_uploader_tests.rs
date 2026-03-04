#![cfg(target_os = "windows")]

use rust_scrcpy::decoder::{D3D11Context, D3D11TextureUploader};

#[test]
fn upload_returns_real_shared_handle() {
    let ctx = D3D11Context::new().expect("create context");
    let mut uploader = D3D11TextureUploader::new_with_context(&ctx).expect("create uploader");

    let data = vec![0x7F; 16 * 16 * 4];
    let handle = uploader
        .upload_bgra_frame(16, 16, 0, &data)
        .expect("upload frame");
    assert!(handle != 0);
}
