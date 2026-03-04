use rust_scrcpy::decoder::pipeline::merge_h264_access_units_for_test;

#[test]
fn packet_merger_caches_config_and_emits_on_media() {
    let sps = vec![0x67, 0x64, 0x00, 0x1F];
    let pps = vec![0x68, 0xEE, 0x3C, 0x80];
    let idr_1 = vec![0x65, 0x80, 0x00];
    let idr_2 = vec![0x65, 0x80, 0x00];

    let out = merge_h264_access_units_for_test(&[sps.clone(), pps.clone(), idr_1.clone(), idr_2]);
    assert_eq!(out.len(), 1);

    let (packet, is_idr) = &out[0];
    let mut expected = Vec::new();
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&sps);
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&pps);
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&idr_1);

    assert_eq!(packet, &expected);
    assert!(*is_idr);
}
