use rust_scrcpy::decode_pts_flags;

#[test]
fn parse_pts_flags_works() {
    let pts: u64 = 123456;
    let pts_flags = (1u64 << 62) | pts;
    let (is_config, is_keyframe, parsed_pts) = decode_pts_flags(pts_flags);

    assert!(!is_config);
    assert!(is_keyframe);
    assert_eq!(parsed_pts, Some(pts));
}
