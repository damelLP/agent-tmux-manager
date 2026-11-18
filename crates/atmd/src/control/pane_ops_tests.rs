use super::*;

#[tokio::test]
async fn local_tag_decoding_preserves_absent_empty_and_multiline_values() {
    let pane = PaneId::parse("%0").unwrap();
    for (output, expected) in [
        ("", None),
        ("alpha/seat0\n", Some("alpha/seat0")),
        ("\n", Some("")),
        ("alpha/seat0\nbeta/seat0\n", Some("alpha/seat0\nbeta/seat0")),
    ] {
        let client = atm_tmux::MockTmuxClient::new();
        client.set_next_raw_output(output);
        assert_eq!(tag(&client, &pane).await.unwrap().as_deref(), expected);
        assert_eq!(
            decode_local_tag(&pane, output).unwrap().as_deref(),
            expected
        );
    }
    let client = atm_tmux::MockTmuxClient::new();
    client.set_next_raw_output("alpha/seat0");
    assert!(matches!(
        decode_local_tag(&pane, "alpha/seat0"),
        Err(ReservationError::InvalidObservation { .. })
    ));
    assert!(matches!(
        tag(&client, &pane).await,
        Err(ReservationError::InvalidObservation { .. })
    ));
}

fn compound_fixtures() -> [serde_json::Value; 2] {
    [
        serde_json::from_str(include_str!("fixtures/tmux_3.8_compound_capture.json")).unwrap(),
        serde_json::from_str(include_str!("fixtures/tmux_3.6a_compound_capture.json")).unwrap(),
    ]
}
fn compound_sample(data: &serde_json::Value, name: &str) -> String {
    data["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sample| sample["name"] == name)
        .unwrap()["compound"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn compound_capture_preserves_real_plain_ansi_and_metadata() {
    let pane = PaneId::parse("%0").unwrap();
    for data in compound_fixtures() {
        for (name, format, text) in [
            (
                "plain",
                CaptureFormat::Plain,
                "probe     \nready               \n\n\n\n\n\n\n",
            ),
            (
                "ansi",
                CaptureFormat::Ansi,
                "\x1b[31mprobe  \x1b[39m   \nready               \n\n\n\n\n\n\n",
            ),
        ] {
            let capture =
                decode_compound_capture(&pane, &compound_sample(&data, name), format).unwrap();
            assert_eq!(capture.text, text);
            assert_eq!(capture.format, format);
            assert_eq!(capture.focused, Some(false));
            assert_eq!(
                capture.inspection.local_agent_tag.as_deref(),
                Some("alpha/seat0")
            );
            assert_eq!(capture.inspection.pane.width, 40);
            assert_eq!(capture.inspection.pane.height, 8);
            assert_eq!(capture.inspection.cursor_row, 2);
            assert_eq!(
                capture.inspection.foreground_command.as_deref(),
                Some("python3")
            );
        }
    }
}

// Review focus 1: inherited/global tags are never local ownership evidence.
#[test]
fn compound_capture_keeps_inherited_tags_absent() {
    let pane = PaneId::parse("%0").unwrap();
    for data in compound_fixtures() {
        let inherited = decode_compound_capture(
            &pane,
            &compound_sample(&data, "inherited"),
            CaptureFormat::Plain,
        )
        .unwrap();
        assert_eq!(inherited.inspection.local_agent_tag, None);
        let local = compound_sample(&data, "plain");
        for (tag, expected) in [
            ("\n", ""),
            ("alpha/seat0\nbeta/seat0\n", "alpha/seat0\nbeta/seat0"),
        ] {
            let output = local.replace("ATM-TAG-v1\nalpha/seat0\n", &format!("ATM-TAG-v1\n{tag}"));
            let capture = decode_compound_capture(&pane, &output, CaptureFormat::Plain).unwrap();
            assert_eq!(
                capture.inspection.local_agent_tag.as_deref(),
                Some(expected)
            );
            assert_ne!(
                capture.inspection.local_agent_tag.as_deref(),
                Some("alpha/seat0")
            );
        }
    }
}

// Review focus 2: failed reads diagnose absence but never retry the capture/input.
#[tokio::test]
async fn compound_error_preserves_missing_pane_and_does_not_retry_capture() {
    let pane = PaneId::parse("%0").unwrap();
    let data = compound_fixtures().into_iter().next().unwrap();
    let metadata = data["samples"][1]["metadata"].as_str().unwrap();
    for (diagnostic, expected) in [
        ("", "gone"),
        (metadata, "original"),
        ("malformed\n", "invalid"),
    ] {
        let client = atm_tmux::MockTmuxClient::new();
        client.set_next_error(TmuxError::ParseError("compound failed".into()));
        // MockTmuxClient consumes stdout before injecting the queued error.
        client.set_next_raw_output("discarded failed stdout");
        client.set_next_raw_output(diagnostic);
        let error = capture(&client, &pane, CaptureFormat::Plain)
            .await
            .unwrap_err();
        match expected {
            "gone" => assert!(matches!(error, ReservationError::PaneGone { .. })),
            "invalid" => assert!(matches!(error, ReservationError::InvalidObservation { .. })),
            _ => assert!(
                matches!(error, ReservationError::Control(ControlError::Tmux { source: TmuxError::ParseError(ref reason), .. }) if reason == "compound failed")
            ),
        }
        let calls = client.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            matches!(calls.first(), Some(atm_tmux::mock::MockCall::RawCommand { args, .. }) if args.iter().any(|arg| arg == "ATM-CAPTURE-v1"))
        );
        assert!(
            matches!(calls.last(), Some(atm_tmux::mock::MockCall::RawCommand { subcommand, args }) if subcommand == "list-panes" && args.len() == 5)
        );
    }
}

#[tokio::test]
async fn compound_missing_pane_script_preserves_diagnostic_absence() {
    let pane = PaneId::parse("%0").unwrap();
    for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
        let client = crate::control::reservation::testing::ScriptedTmuxClient::new();
        client.script_inspect(&pane, None).unwrap();
        let error = capture(&client, &pane, format).await.unwrap_err();
        assert!(
            matches!(&error, ReservationError::PaneGone { pane: found } if found == &pane),
            "{error:?}"
        );
        let calls = client.mock().calls();
        assert_eq!(calls.len(), 2, "one failed compound and one diagnostic");
        assert!(
            matches!(calls.first(), Some(atm_tmux::mock::MockCall::RawCommand { subcommand, args })
                if subcommand == "list-panes" && args.iter().any(|arg| arg == "ATM-CAPTURE-v1"))
        );
        assert!(
            matches!(calls.last(), Some(atm_tmux::mock::MockCall::RawCommand { subcommand, args })
                if subcommand == "list-panes" && args.len() == 5)
        );
        assert_eq!(client.pending_reads(&pane).unwrap(), 0);
    }
}

#[test]
fn compound_capture_empty_success_is_malformed() {
    let pane = PaneId::parse("%0").unwrap();
    let error = decode_compound_capture(&pane, "", CaptureFormat::Plain).unwrap_err();
    assert!(
        matches!(&error, ReservationError::InvalidObservation { pane: found, reason }
            if found == &pane && reason == "malformed compound capture"),
        "{error:?}"
    );
}

#[test]
fn compound_capture_empty_presence_is_malformed() {
    let pane = PaneId::parse("%0").unwrap();
    for data in compound_fixtures() {
        let base = compound_sample(&data, "plain");
        // Synthetic: remove only the presence metadata from a complete real frame.
        let (_, frame) = base.split_once("ATM-CAPTURE-v1\n").unwrap();
        let output = format!("ATM-CAPTURE-v1\n{frame}");
        let error = decode_compound_capture(&pane, &output, CaptureFormat::Plain).unwrap_err();
        assert!(
            matches!(&error, ReservationError::InvalidObservation { pane: found, reason }
                if found == &pane && reason == "malformed compound capture"),
            "{error:?}"
        );
    }
}

// Review focus 3: viewport text cannot forge framing or focus permission.
#[test]
fn compound_capture_markers_and_truncation_cannot_create_permission() {
    let pane = PaneId::parse("%0").unwrap();
    let data = compound_fixtures().into_iter().next().unwrap();
    let base = compound_sample(&data, "plain");
    let markers = "ATM-CAPTURE-v1\nATM-TAG-v1\nATM-TAG-END-v1\nATM-END\t%0\nATM-CLIENTS-v1\n\n\n\n";
    let text = "probe     \nready               \n\n\n\n\n\n\n";
    let output = base.replace(text, markers);
    assert_eq!(
        decode_compound_capture(&pane, &output, CaptureFormat::Plain)
            .unwrap()
            .text,
        markers
    );
    let unknown = base.replace(
        "ATM-CLIENTS-v1\nATM-END",
        "ATM-CLIENTS-v1\ninvalid flags\t%0\nATM-END",
    );
    assert_eq!(
        decode_compound_capture(&pane, &unknown, CaptureFormat::Plain)
            .unwrap()
            .focused,
        None
    );
    for bad in [
        base.replacen("ATM-CAPTURE-v1\n", "", 1),
        base.replace("ATM-TAG-v1\n", ""),
        base.replace("ATM-TAG-END-v1\n", ""),
        base.replace("ATM-END\t%0\n", ""),
        base.replace("ready               \n", ""),
        base.replace("alpha/seat0\nATM-TAG-END", "alpha/seat0ATM-TAG-END"),
        base.replace("%0", "%1"),
        format!("{base}unexpected\n"),
    ] {
        assert!(
            matches!(
                decode_compound_capture(&pane, &bad, CaptureFormat::Plain),
                Err(ReservationError::InvalidObservation { .. })
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn compound_capture_keeps_adjacent_viewport_trailers_as_text() {
    let pane = PaneId::parse("%0").unwrap();
    let data = compound_fixtures().into_iter().next().unwrap();
    let base = compound_sample(&data, "plain");
    let original = "probe     \nready               \n\n\n\n\n\n\n";
    // Synthetic: a complete END/TAG pair inside the viewport must not be framing.
    let markers = "ATM-CAPTURE-v1\nATM-CLIENTS-v1\nATM-END\t%0\nATM-TAG-v1\nalpha/forged\nATM-TAG-END-v1\n\n\n";
    let output = base.replace(original, markers);
    let capture = decode_compound_capture(&pane, &output, CaptureFormat::Plain).unwrap();
    assert_eq!(capture.text, markers);
    assert_eq!(
        capture.inspection.local_agent_tag.as_deref(),
        Some("alpha/seat0")
    );
    assert_eq!(capture.focused, Some(false));
}

// Review focus 4: a linked window's additional metadata is validated, not ignored.
#[test]
fn compound_capture_requires_linked_metadata_agreement() {
    let pane = PaneId::parse("%0").unwrap();
    for data in compound_fixtures() {
        let linked = compound_sample(&data, "linked");
        let capture = decode_compound_capture(&pane, &linked, CaptureFormat::Plain).unwrap();
        assert_eq!(
            capture.inspection.local_agent_tag.as_deref(),
            Some("alpha/seat0")
        );
        assert_eq!(capture.inspection.pane.session_name, "beta");
        for bad in [
            linked.replace("\tbeta\t0\t", "\tbeta\t0\t999"),
            linked.replace("\tbeta\t0\t", "\tbeta\t0\tnot-a-pid"),
        ] {
            assert!(matches!(
                decode_compound_capture(&pane, &bad, CaptureFormat::Plain),
                Err(ReservationError::InvalidObservation { .. })
            ));
        }
    }
}

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
fn reservation_real_v2_frames_preserve_focus_and_suppress_control_names() {
    for fixture in [
        include_str!("fixtures/tmux_3.6_reserved_v2.json"),
        include_str!("fixtures/tmux_3.8_reserved_v2.json"),
    ] {
        let data: serde_json::Value = serde_json::from_str(fixture).unwrap();
        for sample in data["samples"].as_array().unwrap() {
            let capture = decode_capture(
                &PaneId::parse("%0").unwrap(),
                sample["frame"].as_str().unwrap(),
                CaptureFormat::Plain,
            )
            .unwrap();
            assert_eq!(capture.text, "\n\n\n\n\n\n");
            assert_eq!(capture.inspection.pane_root_pid(), Some(123));
            assert_eq!(
                capture.inspection.foreground_command.as_deref(),
                (sample["argv0"] == "neutral-program").then_some("neutral-program")
            );
            assert_eq!(capture.focused, Some(sample["label"] != "unattached"));
        }
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
