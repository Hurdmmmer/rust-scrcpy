use std::ffi::c_void;
use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, PatBlt,
    ReleaseDC, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLACKNESS, DIB_RGB_COLORS, HALFTONE, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::Interface;
use tracing::{debug, warn};

use crate::decoder::gpu_direct_output::context::D3D11Context;

/// D3D11 渲染器（测试窗口版本）。
///
/// 当前实现为了稳定联调，采用 GDI 对 BGRA 直接 blit。
/// 接口保留 `new_with_context`，后续可无缝替换为真实 D3D11 swap-chain。
pub struct D3D11Renderer {
    hwnd: HWND,
    ctx: D3D11Context,
    client_width: u32,
    client_height: u32,
    video_width: u32,
    video_height: u32,
    render_count: u64,
    shared_cache: HashMap<u64, ID3D11Texture2D>,
    shared_mutex_cache: HashMap<u64, IDXGIKeyedMutex>,
    staging_tex: Option<ID3D11Texture2D>,
    staging_size: (u32, u32),
    last_mutex_timeout_log_tick: u64,
}

impl D3D11Renderer {
    pub fn new_with_context(
        hwnd_raw: *mut c_void,
        width: u32,
        height: u32,
        ctx: &D3D11Context,
    ) -> Result<Self, String> {
        let hwnd = HWND(hwnd_raw);
        Ok(Self {
            hwnd,
            ctx: ctx.clone(),
            client_width: width.max(1),
            client_height: height.max(1),
            video_width: width.max(1),
            video_height: height.max(1),
            render_count: 0,
            shared_cache: HashMap::new(),
            shared_mutex_cache: HashMap::new(),
            staging_tex: None,
            staging_size: (0, 0),
            last_mutex_timeout_log_tick: 0,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        self.client_width = width.max(1);
        self.client_height = height.max(1);
        Ok(())
    }

    pub fn set_video_size(&mut self, width: u32, height: u32) {
        self.video_width = width.max(1);
        self.video_height = height.max(1);
    }

    /// 重置共享纹理相关缓存（用于分辨率/方向重配）。
    ///
    /// 背景：
    /// - 旋转后通常会重建解码器与共享纹理；
    /// - 若继续复用旧 handle 的缓存对象，容易出现 keyed mutex 状态错位或资源失效。
    /// 约定：
    /// - 收到 `ReconfigureBegin` 后调用；
    /// - 下一帧到来时会自动按新 handle 重新 OpenSharedResource。
    pub fn reset_shared_resources(&mut self) {
        self.shared_cache.clear();
        self.shared_mutex_cache.clear();
        self.staging_tex = None;
        self.staging_size = (0, 0);
        self.last_mutex_timeout_log_tick = 0;
    }

    /// 直接渲染 CPU BGRA 帧。
    pub fn render_bgra_frame(&mut self, width: u32, height: u32, data: &[u8]) -> Result<(), String> {
        self.render_count = self.render_count.saturating_add(1);
        if width == 0 || height == 0 {
            return Err("invalid frame size".to_string());
        }
        let expect = width
            .checked_mul(height)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| "frame size overflow".to_string())? as usize;
        if data.len() < expect {
            return Err(format!(
                "invalid frame length: got {}, expect at least {}",
                data.len(), expect
            ));
        }

        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rect);
        }
        let cw = ((rect.right - rect.left).max(1)) as u32;
        let ch = ((rect.bottom - rect.top).max(1)) as u32;
        self.client_width = cw;
        self.client_height = ch;

        let (dx, dy, dw, dh) = calc_letterbox(cw, ch, width, height);
        if self.render_count % 120 == 0 {
            debug!(
                "renderer frame: seq={}, src={}x{}, dst={}x{}, letterbox=({},{} {}x{}), bytes={}",
                self.render_count,
                width,
                height,
                cw,
                ch,
                dx,
                dy,
                dw,
                dh,
                data.len()
            );
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        unsafe {
            let hdc = GetDC(self.hwnd);
            if hdc.0.is_null() {
                return Err("GetDC failed".to_string());
            }

            let mem_dc = CreateCompatibleDC(hdc);
            if mem_dc.0.is_null() {
                let _ = ReleaseDC(self.hwnd, hdc);
                return Err("CreateCompatibleDC failed".to_string());
            }

            let back_bmp = CreateCompatibleBitmap(hdc, cw as i32, ch as i32);
            if back_bmp.0.is_null() {
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(self.hwnd, hdc);
                return Err("CreateCompatibleBitmap failed".to_string());
            }

            let old_obj: HGDIOBJ = SelectObject(mem_dc, HGDIOBJ(back_bmp.0));

            let _ = PatBlt(mem_dc, 0, 0, cw as i32, ch as i32, BLACKNESS);
            let _ = SetStretchBltMode(mem_dc, HALFTONE);
            let mut old_origin = windows::Win32::Foundation::POINT::default();
            let _ = SetBrushOrgEx(mem_dc, 0, 0, Some(&mut old_origin));

            let _ = StretchDIBits(
                mem_dc,
                dx,
                dy,
                dw,
                dh,
                0,
                0,
                width as i32,
                height as i32,
                Some(data.as_ptr() as *const _),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );

            let _ = BitBlt(
                hdc,
                0,
                0,
                cw as i32,
                ch as i32,
                mem_dc,
                0,
                0,
                SRCCOPY,
            );

            let _ = SelectObject(mem_dc, old_obj);
            let _ = DeleteObject(HGDIOBJ(back_bmp.0));
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(self.hwnd, hdc);
        }

        Ok(())
    }

    /// 预留接口：真实 GPU 共享句柄渲染。
    pub fn render_shared_handle(&mut self, handle: u64) -> Result<(), String> {
        // 当前测试上下文是占位 D3D11Context，不具备 OpenSharedResource 能力。
        // 保留接口并返回 Ok，避免示例编译/运行被该路径阻断。
        if self.render_count % 120 == 0 {
            warn!(
                "render_shared_handle skipped(handle={}): placeholder D3D11Context has no shared-open support",
                handle
            );
        }
        Ok(())
    }
}

fn calc_letterbox(dst_w: u32, dst_h: u32, src_w: u32, src_h: u32) -> (i32, i32, i32, i32) {
    let dst_ar = dst_w as f32 / dst_h as f32;
    let src_ar = src_w as f32 / src_h as f32;

    let (w, h) = if src_ar > dst_ar {
        let w = dst_w;
        let h = ((dst_w as f32 / src_ar).round() as u32).max(1);
        (w, h)
    } else {
        let h = dst_h;
        let w = ((dst_h as f32 * src_ar).round() as u32).max(1);
        (w, h)
    };

    let x = ((dst_w.saturating_sub(w)) / 2) as i32;
    let y = ((dst_h.saturating_sub(h)) / 2) as i32;
    (x, y, w as i32, h as i32)
}
