#[path = "support/geometry.rs"]
mod geometry;

use geometry::{map_window_touch, Orientation, PointI32, SizeU32};

#[test]
fn map_center_without_rotation_is_center() {
    let mapped = map_window_touch(
        PointI32 { x: 540, y: 1200 },
        SizeU32 {
            width: 1080,
            height: 2400,
        },
        SizeU32 {
            width: 1080,
            height: 2400,
        },
        SizeU32 {
            width: 1080,
            height: 2400,
        },
        Orientation::Deg0,
    )
    .expect("should map");

    assert!(mapped.inside_content);
    assert!((mapped.norm_x - 0.5).abs() < 0.01);
    assert!((mapped.norm_y - 0.5).abs() < 0.01);
}

#[test]
fn map_touch_in_letterbox_marks_outside_content() {
    let mapped = map_window_touch(
        PointI32 { x: 0, y: 10 },
        SizeU32 {
            width: 1920,
            height: 1080,
        },
        SizeU32 {
            width: 1920,
            height: 1080,
        },
        SizeU32 {
            width: 1080,
            height: 2400,
        },
        Orientation::Deg0,
    )
    .expect("should map");

    assert!(!mapped.inside_content);
}

#[test]
fn map_rotation_90_applies_inverse_transform() {
    let mapped = map_window_touch(
        PointI32 { x: 960, y: 540 },
        SizeU32 {
            width: 1920,
            height: 1080,
        },
        SizeU32 {
            width: 1920,
            height: 1080,
        },
        SizeU32 {
            width: 1080,
            height: 1920,
        },
        Orientation::Deg90,
    )
    .expect("should map");

    assert!(mapped.inside_content);
    assert!((mapped.norm_x - 0.5).abs() < 0.05);
    assert!((mapped.norm_y - 0.5).abs() < 0.05);
}
