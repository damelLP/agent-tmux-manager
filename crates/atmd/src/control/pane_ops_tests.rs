use super::*;
fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/tmux_3.8_reserved_observations.json")).unwrap()
}
#[test]
fn reservation_kind_allowlist_excludes_observer_input_and_delivery_destruction() {
    let pane = PaneId::parse("%0").unwrap();
    let agent = "p/a".parse().unwrap();
    let actions = [
        Action::Inspect(pane.clone()),
        Action::Capture(pane.clone(), CaptureFormat::Plain),
        Action::Tag(pane.clone(), ExpectedTag::Absent),
        Action::Untag(pane.clone(), agent),
        Action::Input {
            pane: pane.clone(),
            expected: "p/a".parse().unwrap(),
            input: Input::Key(PaneKey::Enter),
            guard: None,
        },
        Action::Split(
            SpawnContext::new("%0".into(), None, false),
            PaneDirection::Below,
            "50%".into(),
        ),
        Action::Kill(
            pane,
            "p/a".parse().unwrap(),
            serde_json::Value::Null,
            chrono::Utc::now(),
        ),
    ];
    for (kind, allowed) in [
        (
            ReservationKind::Up,
            [true, true, true, false, true, true, false],
        ),
        (
            ReservationKind::Adopt,
            [true, true, true, false, false, false, false],
        ),
        (
            ReservationKind::Clear,
            [true, true, false, false, true, false, false],
        ),
        (
            ReservationKind::Respawn,
            [true, true, true, true, true, true, true],
        ),
        (
            ReservationKind::Down,
            [true, true, false, true, false, false, true],
        ),
        (
            ReservationKind::Delivery,
            [true, true, false, false, true, false, false],
        ),
        (
            ReservationKind::Observe,
            [true, true, false, false, false, false, false],
        ),
    ] {
        for (action, expected) in actions.iter().zip(allowed) {
            assert_eq!(action.allowed(kind), expected, "{kind:?}");
        }
    }
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
fn reservation_focus_accepts_real_key_value_client_flags() {
    for fixture in [
        include_str!("fixtures/tmux_3.6_clients.json"),
        include_str!("fixtures/tmux_3.8_clients.json"),
    ] {
        let data: serde_json::Value = serde_json::from_str(fixture).unwrap();
        for key in ["attached", "paused"] {
            let line = data[key].as_str().unwrap();
            assert_eq!(
                decode_focus(&PaneId::parse("%0").unwrap(), line),
                Some(true)
            );
            assert_eq!(
                decode_focus(&PaneId::parse("%7").unwrap(), line),
                Some(false)
            );
        }
    }
}

#[test]
fn reservation_focus_is_exact_and_malformed_clients_are_unknown() {
    let pane = PaneId::parse("%0").unwrap();
    let data = fixture();
    let base = data["ansi"].as_str().unwrap();
    for (clients, expected) in [
        ("", Some(false)),
        ("focused,UTF-8\t%0\n", Some(true)),
        ("UTF-8,focused\t%9\nUTF-8\t%0\n", Some(false)),
        ("unfocused\t%0\n", Some(false)),
        ("UTF-8\t%0\nfocused\t%0\n", Some(true)),
        ("focused\t%0\nmalformed\n", None),
        ("focused\t%00\n", None),
        ("\t%0\n", None),
        ("#{client_flags}\t%0\n", None),
        ("focused,,UTF-8\t%0\n", None),
        ("focused\t%0", None),
    ] {
        let framed = base.replace("ATM-END", &format!("ATM-CLIENTS-v1\n{clients}ATM-END"));
        let captured = decode_capture(&pane, &framed, CaptureFormat::Ansi).unwrap();
        assert_eq!(captured.focused, expected, "{clients:?}");
        assert!(captured.text.starts_with("\x1b[31mprobe"));
    }
}

#[test]
fn reservation_current_frames_cannot_consume_the_clients_marker_as_a_viewport_row() {
    let pane = PaneId::parse("%0").unwrap();
    let data = fixture();
    let v2 = data["plain"]
        .as_str()
        .unwrap()
        .replacen("ATM-PANE-v1", "ATM-PANE-v2", 1)
        .replacen('\n', "\t\tneutral\n", 1)
        .replace("ATM-END", "ATM-CLIENTS-v1\nATM-END");
    assert_eq!(
        decode_capture(&pane, &v2, CaptureFormat::Plain)
            .unwrap()
            .focused,
        Some(false)
    );
    let truncated = v2.replace("ready     \n", "");
    assert!(matches!(
        decode_capture(&pane, &truncated, CaptureFormat::Plain),
        Err(ReservationError::InvalidObservation { .. })
    ));
}

#[test]
fn reservation_process_facts_are_optional_and_never_classified() {
    let pane = PaneId::parse("%0").unwrap();
    let data = fixture();
    let base = data["filtered_list"].as_str().unwrap().trim_end();
    let v2 = base.replace("ATM-PANE-v1", "ATM-PANE-v2");
    for command in ["neutral-program", "", "name with spaces"] {
        let parsed = decode_metadata(&pane, &format!("{v2}\t\t{command}\n")).unwrap();
        assert_eq!(parsed.pane_root_pid(), Some(2353666));
        assert_eq!(parsed.foreground_pid, None);
        assert_eq!(
            parsed.foreground_command.as_deref(),
            (!command.is_empty()).then_some(command)
        );
    }
    let unsupported =
        decode_metadata(&pane, &format!("{}\t\t\n", v2.replace("2353666", ""))).unwrap();
    assert_eq!(unsupported.pane_root_pid(), None);
    assert_eq!(unsupported.foreground_command, None);
}

#[test]
fn reservation_grouped_panes_agree_and_mode_count_is_not_boolean() {
    let data = fixture();
    let pane = PaneId::parse("%0").unwrap();
    let metadata = data["filtered_list"].as_str().unwrap();
    let first = decode_metadata(&pane, metadata).unwrap();
    let linked = metadata.replace(
        &format!(
            "\t{}\t{}\t",
            first.pane.session_name, first.pane.window_index
        ),
        "\tlinked\t99\t",
    );
    assert_eq!(
        decode_metadata(&pane, &format!("{metadata}{linked}")).unwrap(),
        first
    );
    assert!(
        decode_metadata(&pane, &metadata.replace("\t0\t0\t0\t5", "\t0\t2\t0\t5"))
            .unwrap()
            .in_mode
    );
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
        metadata.replace("\t0\t0\t0\t5", "\t0\tbad\t0\t5"),
        format!("{metadata}{}", metadata.replace("2353666", "2353667")),
        format!("{metadata}{}", metadata.replace("%0", "%1")),
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
