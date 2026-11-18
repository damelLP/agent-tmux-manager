//! Synthetic fixtures only: no harness capture, terminal, daemon, or environment I/O.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test fixtures may panic, as in atm-tui snapshots.
use atm_chat::*;
use atm_comms::message_types::{DeliveryPhase, InboxEntry, InboxResult};
use atm_comms::{Address, AgentStatus, DeliveryStatus, MessageId};
use chrono::{TimeZone, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, buffer::Buffer, style::Color, Terminal};
use std::time::Duration;

fn time(seconds: u64) -> ChatTime {
    ChatTime {
        utc: Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
        elapsed: Duration::from_secs(seconds),
    }
}
fn key(s: &mut ChatState, code: KeyCode, modifiers: KeyModifiers) -> Vec<ChatEffect> {
    s.reduce(ChatInput::Key(KeyEvent::new(code, modifiers)), time(0))
}
fn ch(s: &mut ChatState, c: char) -> Vec<ChatEffect> {
    key(s, KeyCode::Char(c), KeyModifiers::NONE)
}
fn draw(s: &mut ChatState, width: u16, height: u16) -> (Buffer, FrameReport) {
    let _ = s.reduce(ChatInput::Resize { width, height }, time(0));
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut report = None;
    terminal.draw(|f| report = Some(render(f, s))).unwrap();
    (terminal.backend().buffer().clone(), report.unwrap())
}
fn text(b: &Buffer) -> String {
    b.content.iter().map(|c| c.symbol()).collect::<String>()
}
fn fixture() -> (ChatState, Vec<InboxEntry>) {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/chat.json")).unwrap();
    let rows: Vec<InboxEntry> = serde_json::from_value(fixture["rows"].clone()).unwrap();
    let cards = vec![
        SeatCard {
            agent: "alpha/chief".parse().unwrap(),
            state: AgentStatus::Ready,
            binding: SeatBinding {
                pane: Some("%4".into()),
                session_id: Some("opaque-a".into()),
            },
            activity_label: "working".into(),
            activity_color: Color::Blue,
            working: true,
            context_percentage: Some(41.0),
        },
        SeatCard {
            agent: "beta/chief".parse().unwrap(),
            state: AgentStatus::Down,
            binding: SeatBinding::default(),
            activity_label: "down".into(),
            activity_color: Color::Red,
            working: false,
            context_percentage: Some(91.0),
        },
    ];
    let mut s = ChatState::new(ChatConfig {
        initial_thread: Some("alpha/chief".parse().unwrap()),
        caller_pane: Some("%8".into()),
    });
    let _ = s.reduce(
        ChatInput::Connection {
            online: true,
            methods: [
                "org.tree",
                "msg.history",
                "msg.inbox",
                "msg.send",
                "msg.ack",
            ]
            .map(str::to_owned)
            .to_vec(),
            watch_available: true,
        },
        time(0),
    );
    let e = s.reduce(ChatInput::Start, time(0));
    let token = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::LoadThreads { token } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap();
    let e = s.reduce(
        ChatInput::ThreadsLoaded(
            token,
            Ok(ThreadFacts {
                tree_seq: 20,
                seats: cards,
                waiting_decisions: 3,
                decisions_truncated: true,
                caller_bound: false,
            }),
        ),
        time(0),
    );
    for effect in e {
        match effect {
            ChatEffect::LoadInbox { token, .. } => {
                let _ = s.reduce(
                    ChatInput::InboxLoaded(
                        token,
                        Ok(InboxResult {
                            messages: rows
                                .iter()
                                .filter(|r| r.content.to == Address::Owner && r.read_at.is_none())
                                .cloned()
                                .collect(),
                            unread_count: 135,
                            next_after: Some(MessageId::new(1)),
                        }),
                    ),
                    time(0),
                );
            }
            ChatEffect::LoadPage { token, .. } => {
                let _ = s.reduce(
                    ChatInput::PageLoaded(
                        token,
                        Ok(ChatPage {
                            rows: rows.clone(),
                            head: 20,
                            fence: 20,
                            older: Some("opaque-older".into()),
                        }),
                    ),
                    time(0),
                );
            }
            _ => {}
        }
    }
    (s, rows)
}
fn only_row(body: String) -> ChatState {
    let (mut s, rows) = fixture();
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 500,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let token = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::LoadPage { token, .. } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap();
    let mut row = rows.last().unwrap().clone();
    row.message_id = MessageId::new(500);
    row.content.body = body;
    row.content.attachments.clear();
    row.content.from = "alpha/chief".parse().unwrap();
    row.content.to = Address::Owner;
    row.read_at = None;
    row.status = DeliveryStatus::Delivered;
    row.delivery.phase = DeliveryPhase::Delivered;
    let _ = s.reduce(
        ChatInput::PageLoaded(
            token,
            Ok(ChatPage {
                rows: vec![row],
                head: 501,
                fence: 501,
                older: Some("next-window".into()),
            }),
        ),
        time(0),
    );
    s
}

// Break: hiding a canonical delivery label or placing the divider after the first unread.
#[test]
fn all_delivery_labels_and_new_divider() {
    let (mut s, _) = fixture();
    let (b, report) = draw(&mut s, 120, 45);
    let t = text(&b);
    for label in [
        "queued",
        "delivering",
        "held:",
        "delivered",
        "seen (read)",
        "failed",
        "new",
        "first page only",
        "at least 3",
        "summary capped",
    ] {
        assert!(t.contains(label), "missing {label}: {t}");
    }
    let new = t.find("─ new ─").unwrap();
    let reply = t.find("latest answer").unwrap();
    assert!(new < reply);
    assert!(report.can_compose);
    assert!(!report.read_ids.is_empty());
}

// Break: treating a clipped start, a preview, or a trailing attachment as a body read.
#[test]
fn only_last_wrapped_body_line_is_presented_16_kib() {
    let mut s = only_row(format!("{}END", "x".repeat(16381)));
    let (_, first) = draw(&mut s, 40, 12);
    assert!(!first.visible_ids.contains(&MessageId::new(500)));
    let effects = s.reduce(
        ChatInput::Presented {
            generation: first.generation,
            visible_ids: first.visible_ids,
            visible: true,
            hint: None,
        },
        time(0),
    );
    assert!(!effects.iter().any(|e| matches!(e, ChatEffect::Ack { .. })));
    for _ in 0..200 {
        let _ = key(&mut s, KeyCode::Char('d'), KeyModifiers::CONTROL);
    }
    let (b, last) = draw(&mut s, 40, 12);
    assert!(text(&b).contains("END"));
    assert_eq!(last.visible_ids, vec![MessageId::new(500)]);
    let e = s.reduce(
        ChatInput::Presented {
            generation: last.generation,
            visible_ids: last.visible_ids,
            visible: true,
            hint: None,
        },
        time(0),
    );
    assert!(e
        .iter()
        .any(|e| matches!(e,ChatEffect::Ack{params,..}if params.ids==vec![MessageId::new(500)])));
}

// Break: resizing discards the per-thread draft or underflows geometry.
#[test]
fn small_resize_preserves_draft() {
    let (mut s, _) = fixture();
    let _ = ch(&mut s, 'i');
    for c in "draft λ".chars() {
        let _ = ch(&mut s, c);
    }
    let _ = key(&mut s, KeyCode::Char('j'), KeyModifiers::CONTROL);
    let _ = ch(&mut s, 'x');
    for (w, h) in [(0, 0), (1, 1), (4, 2), (20, 5), (40, 12), (80, 24)] {
        let _ = draw(&mut s, w, h);
    }
    let (b, _) = draw(&mut s, 80, 24);
    let t = text(&b);
    assert!(t.contains("draft λ"));
    assert!(t.contains("x"));
    assert!(t.contains("INSERT"));
}

// Break: passing an ESC/control byte through a cell symbol, or altering the original copy.
#[test]
fn controls_are_visible_not_executed() {
    let original = "hello\u{1b}[31m\u{7}\tλ\nnext";
    let mut s = only_row(original.into());
    let (b, _) = draw(&mut s, 100, 28);
    let t = text(&b);
    assert!(!t.chars().any(|c| c.is_control()));
    assert!(t.contains("\\u{1b}[31m"));
    assert!(t.contains("\\u{7}"));
    assert!(t.contains("λ"));
    let e = ch(&mut s, 'y');
    assert!(matches!(e.as_slice(),[ChatEffect::Copy{body}]if body==original));
}

#[test]
fn help_matches_slice_one_keys() {
    let (mut s, _) = fixture();
    let _ = ch(&mut s, '?');
    let (b, _) = draw(&mut s, 100, 28);
    let t = text(&b);
    for key in [
        "j/k",
        "gg/G",
        "Ctrl-d/u",
        "h/l/Tab",
        "J/K",
        "Enter",
        "i",
        "r",
        "/",
        "n/N",
        "y",
        "?",
        "q",
        "Ctrl-j",
        "Esc",
        "Ctrl-r",
        "10s",
        "send may still arrive",
        "pane jump is slice 2",
    ] {
        assert!(t.contains(key), "missing {key}");
    }
    assert!(!t.contains("Enter jumps"));
    assert!(t.contains("prefix C-t"));
}

#[test]
fn down_old_and_stale_are_explicit() {
    let (mut s, _) = fixture();
    let _ = s.reduce(
        ChatInput::Connection {
            online: false,
            methods: vec![],
            watch_available: false,
        },
        time(0),
    );
    let (b, report) = draw(&mut s, 100, 28);
    let t = text(&b);
    assert!(t.contains("disconnected"));
    assert!(t.contains("stale"));
    assert!(t.contains("down"));
    assert!(t.contains("pane missing"));
    assert!(!report.can_compose);
    let _ = s.reduce(
        ChatInput::Connection {
            online: true,
            methods: vec![],
            watch_available: false,
        },
        time(0),
    );
    let (b, report) = draw(&mut s, 80, 24);
    assert!(text(&b).contains("daemon too old"));
    assert!(!report.can_compose);
}

#[test]
fn rendering_is_immutable_and_unfocused_hint_never_claims_read() {
    let (mut s, _) = fixture();
    let (_, report) = draw(&mut s, 100, 28);
    let generation = report.generation;
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    let mut second = None;
    terminal.draw(|f| second = Some(render(f, &s))).unwrap();
    assert_eq!(second.unwrap().generation, generation);
    let _ = s.reduce(
        ChatInput::Presented {
            generation,
            visible_ids: report.visible_ids,
            visible: false,
            hint: Some("auto-ack paused: unfocused".into()),
        },
        time(0),
    );
    let (b, _) = draw(&mut s, 100, 28);
    assert!(text(&b).contains("auto-ack paused: unfocused"));
}

// Break: column movement changes an invisible cursor or opens the wrong full ID.
#[test]
fn column_cursors_are_visible_and_open_the_selected_thread() {
    let (mut s, _) = fixture();
    let _ = ch(&mut s, 'h');
    let _ = ch(&mut s, 'j');
    let (b, _) = draw(&mut s, 120, 32);
    assert!(text(&b).contains("› beta/chief"));
    let _ = key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
    let (b, _) = draw(&mut s, 120, 32);
    assert!(text(&b).starts_with("beta/chief"));
    let _ = ch(&mut s, 'l');
    let _ = ch(&mut s, 'j');
    let (b, _) = draw(&mut s, 120, 32);
    assert!(text(&b).contains("› beta/chief"));
}

// Break: a later copy/read status hides the unresolved late-send warning.
#[test]
fn unknown_warning_survives_status_refresh() {
    let (mut s, _) = fixture();
    let _ = ch(&mut s, 'i');
    let _ = ch(&mut s, 'x');
    let e = key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
    let token = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::Send { token, .. } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap();
    let _ = s.reduce(ChatInput::SendDispatched(token), time(0));
    let _ = s.reduce(
        ChatInput::SendResult(token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(0),
    );
    let _ = s.reduce(
        ChatInput::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        time(10),
    );
    let _ = s.reduce(ChatInput::CopyResult(Ok(())), time(10));
    let (b, _) = draw(&mut s, 100, 28);
    assert!(text(&b).contains("send may still arrive"));
}

#[test]
fn intentional_chat_snapshots() {
    let (mut s, _) = fixture();
    let (_, focused) = draw(&mut s, 120, 32);
    let _ = s.reduce(
        ChatInput::Presented {
            generation: focused.generation,
            visible_ids: focused.visible_ids,
            visible: true,
            hint: None,
        },
        time(0),
    );
    let (b, report) = draw(&mut s, 120, 32);
    insta::assert_debug_snapshot!("chat_talk_120x32", b);
    let _ = s.reduce(
        ChatInput::Presented {
            generation: report.generation,
            visible_ids: report.visible_ids,
            visible: false,
            hint: Some("auto-ack paused: unfocused".into()),
        },
        time(0),
    );
    let (b, _) = draw(&mut s, 100, 28);
    insta::assert_debug_snapshot!("chat_states_100x28", b);
    let _ = ch(&mut s, 'i');
    for c in "draft λ\nsecond line".chars() {
        if c == '\n' {
            let _ = key(&mut s, KeyCode::Char('j'), KeyModifiers::CONTROL);
        } else {
            let _ = ch(&mut s, c);
        }
    }
    let e = key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
    let token = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::Send { token, .. } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap();
    let _ = s.reduce(ChatInput::SendDispatched(token), time(0));
    let _ = s.reduce(
        ChatInput::SendResult(
            token,
            Err(ChatFailure::OutcomeUnknown("socket lost".into())),
        ),
        time(0),
    );
    let _ = s.reduce(
        ChatInput::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        time(10),
    );
    let (b, _) = draw(&mut s, 80, 24);
    insta::assert_debug_snapshot!("chat_insert_80x24", b);
    let mut narrow = only_row(format!("{}END", "x".repeat(16381)));
    let (b, _) = draw(&mut narrow, 40, 12);
    insta::assert_debug_snapshot!("chat_narrow_40x12", b);
    let (mut old, _) = fixture();
    let _ = old.reduce(
        ChatInput::Connection {
            online: true,
            methods: vec![],
            watch_available: false,
        },
        time(0),
    );
    let (b, _) = draw(&mut old, 80, 24);
    insta::assert_debug_snapshot!("chat_too_old_80x24", b);
    let (mut down, _) = fixture();
    let _ = down.reduce(
        ChatInput::Connection {
            online: false,
            methods: vec![],
            watch_available: false,
        },
        time(0),
    );
    let (b, _) = draw(&mut down, 100, 28);
    insta::assert_debug_snapshot!("chat_down_stale_100x28", b);
    let (mut help, _) = fixture();
    let _ = ch(&mut help, '?');
    let (b, _) = draw(&mut help, 100, 28);
    insta::assert_debug_snapshot!("chat_help_100x28", b);
}
