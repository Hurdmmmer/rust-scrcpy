use std::sync::Mutex;

use once_cell::sync::Lazy;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, PeekMessageW,
    PostQuitMessage, RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    HMENU, MSG, PM_REMOVE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SYSTEM_METRICS_INDEX,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WNDCLASSW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

static WINDOW_CLASS_ONCE: Lazy<()> = Lazy::new(register_class);
static MOUSE_EVENTS: Lazy<Mutex<Vec<MouseEvent>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 测试窗口初始缩放比例（相对视频分辨率）。
/// 说明：纯测试窗口不需要和视频像素一比一等大，缩小可显著改善首屏体验与联调效率。
const TEST_WINDOW_INITIAL_SCALE: f32 = 0.42;
/// 测试窗口客户端最小短边，避免窗口过小难以点按调试。
const TEST_WINDOW_MIN_EDGE: u32 = 320;
/// 测试窗口客户端最大长边，避免高分屏默认窗口过大。
const TEST_WINDOW_MAX_EDGE: u32 = 980;

#[derive(Debug, Clone, Copy)]
pub enum MouseEventKind {
    Down,
    Up,
    Move,
}

#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub x: i32,
    pub y: i32,
}

/// 进程级 DPI 感知初始化。
pub fn init_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// 创建测试窗口。
pub unsafe fn create_test_window(title: &str, width: u32, height: u32) -> Result<HWND, String> {
    Lazy::force(&WINDOW_CLASS_ONCE);

    let hinstance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
    let class_name = to_wide("RustWsScrcpyTestWindow");
    let title_w = to_wide(title);

    // 重要：CreateWindowExW 传入的是“窗口外框尺寸”，不是 client 区尺寸。
    // 这里先按 scrcpy 同款策略计算“初始最佳 client 尺寸”：
    // 1) 保持视频内容比例；
    // 2) 受显示器可用区域约束（预留边距）；
    // 3) 再换算外框，避免首帧即出现超大窗口和异常黑边。
    // 先做“初始缩放 + 最小/最大边约束”，再交给 scrcpy 风格最优尺寸函数做最终纠偏。
    let (req_w, req_h) = clamp_initial_client_size(width.max(1), height.max(1));
    let (client_w, client_h) =
        get_optimal_client_size(req_w, req_h, width.max(1), height.max(1), true);

    let style = WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0);
    let mut rect = windows::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: client_w as i32,
        bottom: client_h as i32,
    };
    let _ = AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE::default());
    let outer_w = (rect.right - rect.left).max(1);
    let outer_h = (rect.bottom - rect.top).max(1);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title_w.as_ptr()),
        style,
        100,
        100,
        outer_w,
        outer_h,
        HWND(std::ptr::null_mut()),
        HMENU(std::ptr::null_mut()),
        hinstance,
        None,
    )
    .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

    let _ = ShowWindow(hwnd, SW_SHOW);
    Ok(hwnd)
}

/// 消息泵。
pub fn pump_messages() -> bool {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0, PM_REMOVE).as_bool() {
            if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_QUIT {
                return false;
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
    true
}

pub fn get_client_size(hwnd: HWND) -> Option<(i32, i32)> {
    unsafe {
        let mut rect = windows::Win32::Foundation::RECT::default();
        if GetClientRect(hwnd, &mut rect).is_err() {
            return None;
        }
        Some((rect.right - rect.left, rect.bottom - rect.top))
    }
}

/// 按 scrcpy 风格计算“最佳窗口客户端尺寸”（去黑边 + 限制在显示器内）。
fn get_optimal_client_size(
    current_w: u32,
    current_h: u32,
    content_w: u32,
    content_h: u32,
    within_display_bounds: bool,
) -> (u32, u32) {
    if content_w == 0 || content_h == 0 {
        return (current_w.max(1), current_h.max(1));
    }

    let (mut w, mut h) = (current_w.max(1), current_h.max(1));
    if within_display_bounds {
        // 对齐 scrcpy 的 DISPLAY_MARGINS=96 思路，避免窗口默认过大。
        let max_w = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(SYSTEM_METRICS_INDEX(0))
        }
            .saturating_sub(96)
            .max(320) as u32;
        let max_h = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(SYSTEM_METRICS_INDEX(1))
        }
            .saturating_sub(96)
            .max(240) as u32;
        w = w.min(max_w);
        h = h.min(max_h);
    }

    let optimal = h == w.saturating_mul(content_h) / content_w
        || w == h.saturating_mul(content_w) / content_h;
    if optimal {
        return (w.max(1), h.max(1));
    }

    // keep_width 条件与 scrcpy screen.c 保持一致。
    let keep_width = (content_w as u64) * (h as u64) > (content_h as u64) * (w as u64);
    if keep_width {
        h = (content_h as u64 * w as u64 / content_w as u64).max(1) as u32;
    } else {
        w = (content_w as u64 * h as u64 / content_h as u64).max(1) as u32;
    }
    (w.max(1), h.max(1))
}

/// 计算测试窗口初始 client 尺寸（保比例）。
///
/// 目标：
/// 1. 首次打开不要过大（避免“窗口占屏幕太多”）；
/// 2. 仍保持视频比例，避免拉伸；
/// 3. 保留可操作最小尺寸，避免触控联调困难。
fn clamp_initial_client_size(content_w: u32, content_h: u32) -> (u32, u32) {
    let mut w = ((content_w as f32) * TEST_WINDOW_INITIAL_SCALE).round().max(1.0) as u32;
    let mut h = ((content_h as f32) * TEST_WINDOW_INITIAL_SCALE).round().max(1.0) as u32;

    // 限制最大长边。
    let long_edge = w.max(h);
    if long_edge > TEST_WINDOW_MAX_EDGE {
        let scale = TEST_WINDOW_MAX_EDGE as f32 / long_edge as f32;
        w = ((w as f32) * scale).round().max(1.0) as u32;
        h = ((h as f32) * scale).round().max(1.0) as u32;
    }

    // 限制最小短边。
    let short_edge = w.min(h);
    if short_edge < TEST_WINDOW_MIN_EDGE {
        let scale = TEST_WINDOW_MIN_EDGE as f32 / short_edge.max(1) as f32;
        w = ((w as f32) * scale).round().max(1.0) as u32;
        h = ((h as f32) * scale).round().max(1.0) as u32;
    }

    (w.max(1), h.max(1))
}

/// 在分辨率变化时按 scrcpy 逻辑自动调整窗口尺寸。
///
/// 规则：
/// 1. 先按“旧内容 -> 新内容”等比例缩放当前窗口；
/// 2. 再按新内容比例去黑边；
/// 3. 最终限制在显示器可用区域（保留边距）。
pub fn resize_window_for_content(
    hwnd: HWND,
    old_content: (u32, u32),
    new_content: (u32, u32),
) -> Result<(u32, u32), String> {
    let (old_w, old_h) = old_content;
    let (new_w, new_h) = new_content;
    if old_w == 0 || old_h == 0 || new_w == 0 || new_h == 0 {
        return Err("resize_window_for_content: invalid content size".to_string());
    }

    let (cw, ch) = get_client_size(hwnd).ok_or_else(|| "GetClientRect failed".to_string())?;
    let mut target_w = (cw.max(1) as u64 * new_w as u64 / old_w as u64).max(1) as u32;
    let mut target_h = (ch.max(1) as u64 * new_h as u64 / old_h as u64).max(1) as u32;
    (target_w, target_h) = get_optimal_client_size(target_w, target_h, new_w, new_h, true);

    // client -> outer
    let style = WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0);
    let mut rect = windows::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: target_w as i32,
        bottom: target_h as i32,
    };
    let _ = unsafe { AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE::default()) };
    let outer_w = (rect.right - rect.left).max(1);
    let outer_h = (rect.bottom - rect.top).max(1);
    unsafe {
        SetWindowPos(
            hwnd,
            HWND(std::ptr::null_mut()),
            0,
            0,
            outer_w,
            outer_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .map_err(|e| format!("SetWindowPos failed: {e}"))?;
    }

    get_client_size(hwnd)
        .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32))
        .ok_or_else(|| "GetClientRect after resize failed".to_string())
}

pub fn drain_mouse_events() -> Vec<MouseEvent> {
    let mut out = Vec::new();
    if let Ok(mut q) = MOUSE_EVENTS.lock() {
        out.extend(q.drain(..));
    }
    out
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => push_mouse(MouseEventKind::Down, lparam),
        WM_LBUTTONUP => push_mouse(MouseEventKind::Up, lparam),
        WM_MOUSEMOVE => {
            if (wparam.0 & 0x0001) != 0 {
                push_mouse(MouseEventKind::Move, lparam);
            }
        }
        _ => {}
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn register_class() {
    unsafe {
        let hinstance = match GetModuleHandleW(None) {
            Ok(v) => v,
            Err(_) => return,
        };
        let class_name = to_wide("RustWsScrcpyTestWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    }
}

fn push_mouse(kind: MouseEventKind, lparam: LPARAM) {
    let x = (lparam.0 as i16) as i32;
    let y = ((lparam.0 >> 16) as i16) as i32;
    if let Ok(mut q) = MOUSE_EVENTS.lock() {
        q.push(MouseEvent { kind, x, y });
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
