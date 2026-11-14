use super::*;
fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/tmux_3.8_reserved_observations.json")).unwrap()
}
#[test]
fn reservation_decodes_real_tmux_frames_and_preserves_whitespace_style() {
    let data = fixture();
    let pane = PaneId::parse("%0").unwrap();
    for (key, format, text) in [
        (
            "plain",
            CaptureFormat::Plain,
            "probe     \nready     \n\n\n\n\n\n\n",
        ),
        (
            "ansi",
            CaptureFormat::Ansi,
            "\x1b[31mprobe  \x1b[39m   \nready     \n\n\n\n\n\n\n",
        ),
    ] {
        let capture = decode_capture(&pane, data[key].as_str().unwrap(), format).unwrap();
        assert_eq!(capture.text, text);
        assert_eq!(capture.format, format);
        assert_eq!(capture.inspection.cursor_column, 5);
        assert_eq!(capture.inspection.cursor_row, 1);
        assert_eq!(capture.inspection.pane.height, 8);
    }
}
#[test]
fn reservation_rejects_malformed_metadata_and_incomplete_framing() {
    let data = fixture();
    let pane = PaneId::parse("%0").unwrap();
    let metadata = data["filtered_list"].as_str().unwrap();
    for bad in [
        metadata.replace("2353666", "not-a-pid"),
        metadata.replace("\t40\t", "\t0\t"),
        metadata.replace("\t5\t1\n", "\t5\t8\n"),
        metadata.replace("%0", "%00"),
        metadata.replace("\t0\t0\t0\t5", "\t0\t2\t0\t5"),
        format!("{metadata}{metadata}"),
        data["missing_display"].as_str().unwrap().into(),
        metadata.trim_end().into(),
    ] {
        assert!(
            matches!(
                decode_metadata(&pane, &bad),
                Err(ReservationError::InvalidObservation { .. })
            ),
            "{bad:?}"
        );
    }
    let frame = data["plain"].as_str().unwrap();
    for bad in [
        frame.replace("ATM-END\t%0\n", ""),
        frame.replace("ready     \n", ""),
        frame.replace("ATM-END\t%0", "ATM-END\t%9"),
        frame.replace("probe", "\0probe"),
    ] {
        assert!(
            matches!(
                decode_capture(&pane, &bad, CaptureFormat::Plain),
                Err(ReservationError::InvalidObservation { .. })
            ),
            "{bad:?}"
        );
    }
}
