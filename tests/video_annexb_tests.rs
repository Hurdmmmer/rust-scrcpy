use rust_scrcpy::extract_annexb_nal;

#[test]
fn annexb_extract_supports_3byte_start_code() {
    let buf = [
        0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, 0x00, 0x00, 0x01, 0x68, 0xCC,
    ];
    let (nal, consumed) = extract_annexb_nal(&buf).expect("should extract");
    assert_eq!(nal, vec![0x67, 0xAA, 0xBB]);
    assert_eq!(consumed, 6);
}

#[test]
fn annexb_extract_supports_4byte_start_code() {
    let buf = [
        0x00, 0x00, 0x00, 0x01, 0x67, 0x11, 0x22, 0x00, 0x00, 0x00, 0x01, 0x68,
    ];
    let (nal, consumed) = extract_annexb_nal(&buf).expect("should extract");
    assert_eq!(nal, vec![0x67, 0x11, 0x22]);
    assert_eq!(consumed, 7);
}

#[test]
fn annexb_extract_drops_noise_prefix() {
    let buf = [
        0xFF, 0xEE, 0x00, 0x00, 0x01, 0x67, 0x12, 0x00, 0x00, 0x01, 0x68,
    ];
    let (nal, consumed) = extract_annexb_nal(&buf).expect("should advance");
    assert!(nal.is_empty());
    assert_eq!(consumed, 2);
}
