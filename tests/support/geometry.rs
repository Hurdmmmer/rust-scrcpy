#![allow(dead_code)]

/// 投屏几何映射模块（测试/示例共享实现）。
///
/// 目标：
/// 1. 将窗口坐标映射到视频内容区域；
/// 2. 处理 letterbox（黑边）带来的可点击区域差异；
/// 3. 在存在渲染旋转时，将触控坐标反算回设备坐标。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointI32 {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeU32 {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

#[derive(Debug, Clone, Copy)]
pub struct TouchMapping {
    /// 归一化 X（设备坐标系）。
    pub norm_x: f32,
    /// 归一化 Y（设备坐标系）。
    pub norm_y: f32,
    /// 触点是否落在有效内容区（非黑边）。
    pub inside_content: bool,
}

/// 窗口触点映射到设备归一化坐标。
///
/// 参数说明：
/// - `window_size`：窗口逻辑尺寸；
/// - `drawable_size`：实际可绘制像素尺寸（HiDPI 下可能 != window_size）；
/// - `frame_size`：当前视频帧尺寸（未旋转时）；
/// - `orientation`：渲染时应用的旋转角。
pub fn map_window_touch(
    p: PointI32,
    window_size: SizeU32,
    drawable_size: SizeU32,
    frame_size: SizeU32,
    orientation: Orientation,
) -> Option<TouchMapping> {
    if window_size.width == 0
        || window_size.height == 0
        || drawable_size.width == 0
        || drawable_size.height == 0
        || frame_size.width == 0
        || frame_size.height == 0
    {
        return None;
    }

    let sx = drawable_size.width as f32 / window_size.width as f32;
    let sy = drawable_size.height as f32 / window_size.height as f32;
    let dx = p.x as f32 * sx;
    let dy = p.y as f32 * sy;

    let content = calc_content_rect(drawable_size, frame_size, orientation);

    let inside = dx >= content.x
        && dx <= content.x + content.w
        && dy >= content.y
        && dy <= content.y + content.h;

    let mut u = (dx - content.x) / content.w.max(1.0);
    let mut v = (dy - content.y) / content.h.max(1.0);
    u = u.clamp(0.0, 1.0);
    v = v.clamp(0.0, 1.0);

    let (nx, ny) = match orientation {
        Orientation::Deg0 => (u, v),
        Orientation::Deg90 => (v, 1.0 - u),
        Orientation::Deg180 => (1.0 - u, 1.0 - v),
        Orientation::Deg270 => (1.0 - v, u),
    };

    Some(TouchMapping {
        norm_x: nx.clamp(0.0, 1.0),
        norm_y: ny.clamp(0.0, 1.0),
        inside_content: inside,
    })
}

#[derive(Debug, Clone, Copy)]
struct RectF {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn calc_content_rect(drawable: SizeU32, frame: SizeU32, orientation: Orientation) -> RectF {
    let (src_w, src_h) = match orientation {
        Orientation::Deg90 | Orientation::Deg270 => (frame.height as f32, frame.width as f32),
        _ => (frame.width as f32, frame.height as f32),
    };

    let dst_w = drawable.width as f32;
    let dst_h = drawable.height as f32;

    let src_ar = src_w / src_h.max(1.0);
    let dst_ar = dst_w / dst_h.max(1.0);

    let (w, h) = if src_ar > dst_ar {
        let w = dst_w;
        let h = (dst_w / src_ar).max(1.0);
        (w, h)
    } else {
        let h = dst_h;
        let w = (dst_h * src_ar).max(1.0);
        (w, h)
    };

    RectF {
        x: (dst_w - w) * 0.5,
        y: (dst_h - h) * 0.5,
        w,
        h,
    }
}
