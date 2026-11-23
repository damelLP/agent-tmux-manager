//! Synthetic: neutral canonical rows exercise the pure reducer; no harness capture.
use super::*;
use atm_comms::message_types::{DeliveryPhase, DeliveryView, InboxResult, SendResult};
use atm_comms::{AgentStatus, DeliveryStatus, MessageContent, MessageKind};
use chrono::{TimeZone, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Color;
use std::time::Duration;

fn time(seconds: u64) -> ChatTime {
    ChatTime {
        utc: Utc
            .timestamp_opt(1_800_000_000 + seconds as i64, 0)
            .unwrap(),
        elapsed: Duration::from_secs(seconds),
    }
}
fn address(s: &str) -> Address {
    s.parse().unwrap()
}
fn seat(name: &str) -> SeatCard {
    SeatCard {
        agent: name.parse().unwrap(),
        state: AgentStatus::Ready,
        binding: SeatBinding {
            pane: Some("%4".into()),
            session_id: Some("opaque-one".into()),
        },
        activity_label: "idle".into(),
        activity_color: Color::Green,
        working: false,
        context_percentage: Some(41.0),
    }
}
fn facts(bound: bool) -> ThreadFacts {
    ThreadFacts {
        tree_seq: 8,
        seats: vec![seat("alpha/chief"), seat("beta/chief")],
        waiting_decisions: 2,
        decisions_truncated: false,
        caller_bound: bound,
    }
}
fn row(id: u64, from: &str, to: &str, body: &str) -> InboxEntry {
    InboxEntry {
        message_id: MessageId::new(id),
        content: MessageContent {
            from: address(from),
            to: address(to),
            kind: MessageKind::Note,
            body: body.into(),
            attachments: vec![],
            task: None,
            delivery_notice: None,
            post: None,
        },
        sent_at: time(0).utc,
        status: DeliveryStatus::Delivered,
        read_at: None,
        delivery: DeliveryView {
            phase: DeliveryPhase::Delivered,
            reason: None,
            attempt: 1,
            pane: Some("%99".into()),
            session_id: None,
            confirmation: None,
            retry_at: None,
            blocked_by: None,
        },
        post_context: None,
    }
}
fn page(rows: Vec<InboxEntry>, head: u64, older: Option<&str>) -> ChatPage {
    ChatPage {
        rows,
        head,
        fence: head,
        older: older.map(str::to_owned),
    }
}
fn key(
    state: &mut ChatState,
    code: KeyCode,
    modifiers: KeyModifiers,
    seconds: u64,
) -> Vec<ChatEffect> {
    state.reduce(
        ChatInput::Key(KeyEvent::new(code, modifiers)),
        time(seconds),
    )
}
fn plain(state: &mut ChatState, c: char) -> Vec<ChatEffect> {
    key(state, KeyCode::Char(c), KeyModifiers::NONE, 0)
}
fn tree_token(e: &[ChatEffect]) -> OpToken {
    e.iter()
        .find_map(|e| {
            if let ChatEffect::LoadThreads { token } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap()
}
fn page_token(e: &[ChatEffect]) -> OpToken {
    e.iter()
        .find_map(|e| {
            if let ChatEffect::LoadPage { token, .. } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap()
}
fn summary_token(e: &[ChatEffect]) -> OpToken {
    e.iter()
        .find_map(|e| {
            if let ChatEffect::LoadInbox { token, params } = e {
                params.id.is_none().then_some(*token)
            } else {
                None
            }
        })
        .unwrap()
}
fn connection(state: &mut ChatState, online: bool) -> Vec<ChatEffect> {
    state.reduce(
        ChatInput::Connection {
            online,
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
    )
}
fn ready(rows: Vec<InboxEntry>) -> ChatState {
    let mut state = ChatState::new(ChatConfig {
        initial_thread: Some(address("alpha/chief")),
        caller_pane: Some("%8".into()),
    });
    let _ = connection(&mut state, true);
    let e = state.reduce(ChatInput::Start, time(0));
    let e = state.reduce(
        ChatInput::ThreadsLoaded(tree_token(&e), Ok(facts(false))),
        time(0),
    );
    let pt = page_token(&e);
    let it = summary_token(&e);
    let _ = state.reduce(
        ChatInput::InboxLoaded(
            it,
            Ok(InboxResult {
                messages: vec![],
                unread_count: 0,
                next_after: None,
            }),
        ),
        time(0),
    );
    let _ = state.reduce(ChatInput::PageLoaded(pt, Ok(page(rows, 20, None))), time(0));
    state
}
fn mutations(e: &[ChatEffect]) -> usize {
    e.iter()
        .filter(|e| matches!(e, ChatEffect::Send { .. } | ChatEffect::Ack { .. }))
        .count()
}
fn present(state: &mut ChatState, ids: Vec<MessageId>, visible: bool) -> Vec<ChatEffect> {
    present_at(state, ids, visible, 0)
}
fn enter_body(state: &mut ChatState, body: &str) -> Vec<ChatEffect> {
    let _ = plain(state, 'i');
    for c in body.chars() {
        let _ = plain(state, c);
    }
    key(state, KeyCode::Enter, KeyModifiers::NONE, 0)
}
fn send_token(e: &[ChatEffect]) -> OpToken {
    e.iter()
        .find_map(|e| {
            if let ChatEffect::Send { token, .. } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap()
}

// Break: allowing Owner mutations without confirmed unbound attribution.
#[test]
fn bound_or_unknown_attribution_never_sends_or_acks() {
    let mut s = ChatState::new(ChatConfig::default());
    let e = s.reduce(ChatInput::Start, time(0));
    let e = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&e), Ok(facts(true))),
        time(0),
    );
    assert!(
        matches!(e.as_slice(),[ChatEffect::Quit {error:Some(ChatFailure::Refused(text))}] if text=="chat runs from an unbound pane")
    );
    assert_eq!(mutations(&enter_body(&mut s, "hello")), 0);
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(1)], true)),
        0
    );
    let mut s = ready(vec![]);
    let e = enter_body(&mut s, "  literal λ  ");
    assert!(
        matches!(e.as_slice(),[ChatEffect::Send {params,..}] if params.to==address("alpha/chief") && params.body=="  literal λ  ")
    );
    let mut s = ready(vec![]);
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "agent.bound".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(
            tree_token(&e),
            Err(ChatFailure::Unavailable("tree unavailable".into())),
        ),
        time(0),
    );
    assert_eq!(mutations(&enter_body(&mut s, "not sent")), 0);
}

// Break: treating a hold as a durable fact, or substituting the current pane.
#[test]
fn status_uses_committed_facts_and_live_inbox_overlay() {
    let mut r = row(9, "owner", "alpha/chief", "work");
    r.status = DeliveryStatus::Queued;
    r.delivery.phase = DeliveryPhase::Queued;
    let mut s = ready(vec![r.clone()]);
    assert!(delivery_label(&r).contains("queued"));
    let _ = present(&mut s, vec![r.message_id], true);
    let e = s.reduce(ChatInput::Tick, time(1));
    let token = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::LoadInbox { token, params } = e {
                (params.id == Some(r.message_id)).then_some(*token)
            } else {
                None
            }
        })
        .unwrap();
    r.delivery.phase = DeliveryPhase::Waiting;
    r.delivery.reason = Some("editor not empty".into());
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            token,
            Ok(InboxResult {
                messages: vec![r],
                unread_count: 0,
                next_after: None,
            }),
        ),
        time(1),
    );
    let got = s.row(&MessageId::new(9)).unwrap();
    assert!(delivery_label(got).contains("held: editor not empty"));
    assert_eq!(got.delivery.pane.as_deref(), Some("%99"));
    let e = s.reduce(ChatInput::Tick, time(2));
    let token = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::LoadInbox { token, params } = e {
                params.id.map(|_| *token)
            } else {
                None
            }
        })
        .unwrap();
    let _ = s.reduce(
        ChatInput::InboxLoaded(token, Err(ChatFailure::Unavailable("offline".into()))),
        time(2),
    );
    assert!(s.status.contains("stale"));
    for (phase, status, read, want) in [
        (
            DeliveryPhase::Delivering,
            DeliveryStatus::Delivering,
            false,
            "delivering",
        ),
        (
            DeliveryPhase::Delivered,
            DeliveryStatus::Delivered,
            false,
            "delivered",
        ),
        (
            DeliveryPhase::Delivered,
            DeliveryStatus::Delivered,
            true,
            "seen (read)",
        ),
        (
            DeliveryPhase::Failed,
            DeliveryStatus::Failed,
            false,
            "failed",
        ),
        (
            DeliveryPhase::Blocked,
            DeliveryStatus::Queued,
            false,
            "held",
        ),
    ] {
        let mut r = row(1, "owner", "alpha/chief", "x");
        r.delivery.phase = phase;
        r.status = status;
        r.read_at = read.then_some(time(1).utc);
        assert!(delivery_label(&r).contains(want), "{want}");
    }
}

// Break: acknowledging previews, non-Owner rows, stale frames, or repeating in-flight IDs.
#[test]
fn only_visible_delivered_owner_rows_are_acked_once() {
    let mut queued = row(2, "alpha/chief", "owner", "queued");
    queued.status = DeliveryStatus::Queued;
    let mut failed = row(3, "alpha/chief", "owner", "failed");
    failed.status = DeliveryStatus::Failed;
    let mut read = row(4, "alpha/chief", "owner", "read");
    read.read_at = Some(time(1).utc);
    let rows = vec![
        row(1, "alpha/chief", "owner", "last line"),
        queued,
        failed,
        read,
        row(5, "owner", "alpha/chief", "out"),
        row(6, "beta/chief", "alpha/chief", "third party"),
    ];
    let mut s = ready(rows);
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(1)], false)),
        0
    );
    assert_eq!(mutations(&present(&mut s, vec![], true)), 0);
    let e = s.reduce(
        ChatInput::Presented {
            generation: s.generation.saturating_sub(1),
            visible_ids: vec![MessageId::new(1)],
            visible: true,
            hint: None,
        },
        time(0),
    );
    assert_eq!(mutations(&e), 0);
    let e = present(&mut s, (1..=6).map(MessageId::new).collect(), true);
    let (token, ids) = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::Ack { token, params } = e {
                Some((*token, params.ids.clone()))
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(ids, vec![MessageId::new(1)]);
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(1)], true)),
        0
    );
    let _ = s.reduce(
        ChatInput::AckResult(token, Err(ChatFailure::OutcomeUnknown("lost reply".into()))),
        time(0),
    );
    assert!(s.row(&MessageId::new(1)).unwrap().read_at.is_none());
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(1)], true)),
        0
    );
}

#[test]
fn long_body_partial_report_never_acks() {
    let mut s = ready(vec![row(1, "alpha/chief", "owner", &"x".repeat(16384))]);
    let _ = s.reduce(
        ChatInput::Resize {
            width: 40,
            height: 12,
        },
        time(0),
    );
    assert_eq!(mutations(&present(&mut s, vec![], true)), 0);
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(1)], true)),
        1
    );
}

// Break: lexicographic ordering, blind append, or applying old-scope responses.
#[test]
fn late_pages_and_replay_do_not_lose_rows_or_selection() {
    let mut s = ready(vec![
        row(10, "alpha/chief", "owner", "ten"),
        row(9, "alpha/chief", "owner", "nine"),
    ]);
    let _ = plain(&mut s, 'g');
    let _ = plain(&mut s, 'g');
    let selected = s.view().selected;
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let pt = page_token(&e);
    assert!(s
        .reduce(
            ChatInput::WatchEvent {
                seq: 21,
                kind: "msg.sent".into()
            },
            time(0)
        )
        .is_empty());
    let _ = s.reduce(
        ChatInput::PageLoaded(
            pt,
            Ok(page(
                vec![
                    row(11, "owner", "alpha/chief", "eleven"),
                    row(10, "alpha/chief", "owner", "ten"),
                ],
                22,
                None,
            )),
        ),
        time(0),
    );
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![9, 10, 11]
    );
    assert_eq!(s.view().selected, selected);
    let generation = s.generation;
    let _ = s.reduce(
        ChatInput::PageLoaded(pt, Ok(page(vec![], 22, None))),
        time(0),
    );
    assert_eq!(s.generation, generation);
}

#[test]
fn nonoverlapping_head_restarts_cached_window() {
    let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
    s.view_mut().older = Some("old-below-600".into());
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 901,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![
                    row(900, "alpha/chief", "owner", "new"),
                    row(801, "alpha/chief", "owner", "new low"),
                ],
                901,
                Some("head-below-801"),
            )),
        ),
        time(0),
    );
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![801, 900]
    );
    assert_eq!(s.view().older.as_deref(), Some("head-below-801"));
    assert!(s.status.contains("history reloaded"));
}

// Break: unlocking/auto-resending after an empty read or accepting an old/unrelated row.
#[test]
fn send_success_refusal_and_unknown_are_distinct() {
    let mut s = ready(vec![]);
    let e = enter_body(&mut s, "hello world");
    let token = send_token(&e);
    let e = s.reduce(
        ChatInput::SendResult(
            token,
            Ok(SendResult {
                message_id: MessageId::new(21),
            }),
        ),
        time(0),
    );
    assert_eq!(e.iter().filter(|e|matches!(e,ChatEffect::RecordSend{token:t,message_id,word_count:2}if *t==token && message_id.seq()==21)).count(),1);
    assert!(s.view().draft.is_empty());
    assert!(!s.status.contains("delivered"));
    let mut s = ready(vec![]);
    let e = enter_body(&mut s, "retained");
    let token = send_token(&e);
    let _ = s.reduce(
        ChatInput::SendResult(token, Err(ChatFailure::Refused("not sent: bound".into()))),
        time(0),
    );
    assert_eq!(s.view().draft, "retained");
    assert!(!s.draft_locked());
    let mut s = ready(vec![]);
    let e = enter_body(&mut s, "identical body");
    let token = send_token(&e);
    let _ = s.reduce(ChatInput::SendDispatched(token), time(0));
    let e = s.reduce(
        ChatInput::SendResult(
            token,
            Err(ChatFailure::OutcomeUnknown("disconnected".into())),
        ),
        time(0),
    );
    assert!(s.draft_locked());
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![
                    row(19, "owner", "alpha/chief", "identical body"),
                    row(21, "beta/chief", "alpha/chief", "identical body"),
                    row(22, "owner", "beta/chief", "identical body"),
                    row(23, "owner", "alpha/chief", "different"),
                ],
                23,
                None,
            )),
        ),
        time(1),
    );
    assert!(s.draft_locked());
    assert_eq!(
        mutations(&key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 1)),
        0
    );
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 25,
            kind: "msg.sent".into(),
        },
        time(3),
    );
    let recovery = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::LoadPage {
                token: read_token,
                query,
            } = e
            {
                (query.text.is_none()
                    && query.cursor.is_none()
                    && s.reads.get(read_token).is_some_and(
                        |p| matches!(p.purpose,ReadPurpose::Reconcile{send,..} if send==token),
                    ))
                .then_some(*read_token)
            } else {
                None
            }
        })
        .unwrap();
    let e = s.reduce(
        ChatInput::PageLoaded(
            recovery,
            Ok(page(
                vec![row(24, "owner", "alpha/chief", "identical body")],
                25,
                None,
            )),
        ),
        time(3),
    );
    assert!(!s.draft_locked());
    assert_eq!(e.iter().filter(|e|matches!(e,ChatEffect::RecordSend{token:t,message_id,word_count:2}if *t==token && message_id.seq()==24)).count(),1);
    assert!(s
        .reduce(
            ChatInput::SendResult(
                token,
                Ok(SendResult {
                    message_id: MessageId::new(24)
                })
            ),
            time(3)
        )
        .is_empty());
}

#[test]
fn unknown_unlock_uses_dispatch_elapsed_not_utc_and_never_sends() {
    let mut s = ready(vec![]);
    let e = enter_body(&mut s, "may arrive");
    let token = send_token(&e);
    let _ = s.reduce(ChatInput::SendDispatched(token), time(50));
    let _ = s.reduce(
        ChatInput::SendResult(token, Err(ChatFailure::OutcomeUnknown("unknown".into()))),
        time(51),
    );
    let mut backward = time(59);
    backward.utc = time(0).utc;
    assert!(s
        .reduce(ChatInput::Tick, backward)
        .iter()
        .all(|e| !matches!(e, ChatEffect::Send { .. })));
    assert_eq!(
        mutations(&key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL, 59)),
        0
    );
    assert!(s.draft_locked());
    let e = key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL, 60);
    assert_eq!(mutations(&e), 0);
    assert!(!s.draft_locked());
    assert_eq!(s.view().draft, "may arrive");
    assert!(s.status.contains("send may still arrive"));
}

// Break: last-component IDs, leaking third-party rows, or System/self OR-query rows.
#[test]
fn query_targets_and_endpoint_pairs_are_literal() {
    let mut s = ready(vec![
        row(1, "alpha/chief", "owner", "yes"),
        row(2, "beta/chief", "owner", "no"),
        row(3, "alpha/chief", "system", "no"),
        row(4, "alpha/chief", "alpha/chief", "no"),
    ]);
    assert_eq!(
        s.view().ids.iter().copied().collect::<Vec<_>>(),
        vec![MessageId::new(1)]
    );
    let _ = plain(&mut s, 'r');
    assert_eq!(s.thread, ThreadKey::Direct(address("alpha/chief")));
    s.open_thread(ThreadKey::All, &mut vec![]);
    assert_eq!(mutations(&enter_body(&mut s, "no")), 0);
    s.open_thread(ThreadKey::Direct(Address::System), &mut vec![]);
    let pending = s
        .reads
        .values()
        .find(|r| {
            matches!(
                &r.purpose,
                ReadPurpose::Page {
                    thread: ThreadKey::Direct(Address::System),
                    ..
                }
            )
        })
        .unwrap()
        .token;
    let e = s.reduce(
        ChatInput::PageLoaded(
            pending,
            Ok(page(
                vec![row(5, "system", "alpha/chief", "unrelated")],
                6,
                Some("next"),
            )),
        ),
        time(0),
    );
    assert!(e.iter().any(
        |e| matches!(e,ChatEffect::LoadPage{query,..}if query.cursor.as_deref()==Some("next"))
    ));
}

#[test]
fn editor_keys_and_byte_boundaries_are_literal() {
    let mut s = ready(vec![]);
    let _ = plain(&mut s, 'i');
    let _ = plain(&mut s, 'λ');
    let _ = key(&mut s, KeyCode::Char('j'), KeyModifiers::CONTROL, 0);
    let _ = plain(&mut s, 'x');
    let _ = key(&mut s, KeyCode::Esc, KeyModifiers::NONE, 0);
    assert_eq!(s.view().draft, "λ\nx");
    assert_eq!(s.mode, Mode::Normal);
    let _ = s.reduce(
        ChatInput::Resize {
            width: 0,
            height: 0,
        },
        time(0),
    );
    assert_eq!(s.view().draft, "λ\nx");
    s.view_mut().draft = "x".repeat(16384);
    let _ = plain(&mut s, 'i');
    let _ = plain(&mut s, 'y');
    assert_eq!(s.view().draft.len(), 16384);
    let e = key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 0);
    assert_eq!(mutations(&e), 1);
    let mut s = ready(vec![]);
    let _ = plain(&mut s, '/');
    for _ in 0..128 {
        let _ = plain(&mut s, 'λ');
    }
    let _ = plain(&mut s, 'x');
    assert_eq!(s.search_edit.len(), 256);
    let e = key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 0);
    assert!(e.iter().any(|e|matches!(e,ChatEffect::LoadPage{query,..} if query.text.as_ref().is_some_and(|s|s.len()==256))));
    let generation = s.generation;
    let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert!(s.reduce(ChatInput::Key(release), time(0)).is_empty());
    assert_eq!(s.generation, generation);
    let mut s = ready(vec![
        row(10, "alpha/chief", "owner", "ten"),
        row(9, "alpha/chief", "owner", "nine"),
    ]);
    let _ = plain(&mut s, 'g');
    let _ = s.reduce(ChatInput::Tick, time(100));
    let _ = plain(&mut s, 'g');
    assert_eq!(s.view().selected, Some(MessageId::new(9)));
}

// Break: starting another watch or full scans on every message/tick.
#[test]
fn one_watch_and_topic_scoped_refresh() {
    let mut s = ChatState::new(ChatConfig::default());
    let _ = connection(&mut s, true);
    let e = s.reduce(ChatInput::Start, time(0));
    let e = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&e), Ok(facts(false))),
        time(0),
    );
    assert_eq!(
        e.iter()
            .filter(|e| matches!(e, ChatEffect::Watch { .. }))
            .count(),
        1
    );
    let e = s.reduce(
        ChatInput::InboxLoaded(
            summary_token(&e),
            Ok(InboxResult {
                messages: vec![row(1, "alpha/chief", "owner", "unread")],
                unread_count: 150,
                next_after: Some(MessageId::new(1)),
            }),
        ),
        time(0),
    );
    assert!(!e.iter().any(|e| matches!(e, ChatEffect::LoadInbox { .. })));
    assert_eq!(s.unread_count, 150);
    assert!(s.summary_incomplete);
    assert_eq!(s.unread_by_sender.get(&address("alpha/chief")), Some(&1));
    let mut s = ready(vec![]);
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "msg.read".into(),
        },
        time(0),
    );
    assert!(!e
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadThreads { .. } | ChatEffect::Watch { .. })));
    assert!(e.iter().any(|e|matches!(e,ChatEffect::LoadInbox{params,..}if params.limit==100 && params.recipient==Some(Address::Owner) && params.after.is_none())));
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "decision.requested".into(),
        },
        time(0),
    );
    assert!(matches!(e.as_slice(), [ChatEffect::LoadThreads { .. }]));
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&e), Ok(facts(false))),
        time(0),
    );
    let _ = connection(&mut s, false);
    let e = connection(&mut s, true);
    assert!(!e.iter().any(|e| matches!(e, ChatEffect::Watch { .. })));
    let e = s.reduce(ChatInput::Tick, time(500));
    assert!(!e.iter().any(|e| matches!(
        e,
        ChatEffect::LoadThreads { .. } | ChatEffect::LoadPage { .. } | ChatEffect::LoadInbox { .. }
    )));
}

#[test]
fn slow_reads_have_one_dirty_followup_per_purpose() {
    let mut s = ready(vec![]);
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let token = page_token(&e);
    for seq in 22..100 {
        assert!(s
            .reduce(
                ChatInput::WatchEvent {
                    seq,
                    kind: "msg.read".into()
                },
                time(0)
            )
            .is_empty());
    }
    let e = s.reduce(
        ChatInput::PageLoaded(token, Ok(page(vec![], 100, None))),
        time(0),
    );
    assert_eq!(
        e.iter()
            .filter(|e| matches!(e, ChatEffect::LoadPage { .. }))
            .count(),
        1
    );
}

#[test]
fn generation_is_reducer_owned() {
    let mut s = ready(vec![row(1, "alpha/chief", "owner", "one")]);
    let before = s.generation;
    let _ = s.reduce(ChatInput::Tick, time(0));
    assert_eq!(s.generation, before);
    let _ = s.reduce(
        ChatInput::Resize {
            width: 100,
            height: 28,
        },
        time(0),
    );
    assert_eq!(s.generation, before + 1);
    let _ = s.reduce(
        ChatInput::Resize {
            width: 100,
            height: 28,
        },
        time(0),
    );
    assert_eq!(s.generation, before + 1);
    let _ = s.reduce(
        ChatInput::PageLoaded(OpToken(u64::MAX), Ok(page(vec![], 1, None))),
        time(0),
    );
    assert_eq!(s.generation, before + 1);
    s.next_token = u64::MAX;
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 30,
            kind: "agent.ready".into(),
        },
        time(0),
    );
    assert!(e.is_empty());
    assert!(s.status.contains("exhausted"));
}

// Break: carrying turn inference across a coalesced rebind or missing session.
#[test]
fn binding_comparison_and_capped_decision_summary() {
    let mut s = ready(vec![]);
    let mut working = seat("alpha/chief");
    working.working = true;
    let _ = s.reduce(ChatInput::SessionUpdate(vec![working.clone()]), time(0));
    let mut idle = working.clone();
    idle.working = false;
    idle.binding.session_id = Some("opaque-two".into());
    let _ = s.reduce(ChatInput::SessionUpdate(vec![idle.clone()]), time(0));
    assert!(s.turn_notes.is_empty());
    idle.working = true;
    let _ = s.reduce(ChatInput::SessionUpdate(vec![idle.clone()]), time(0));
    idle.working = false;
    let _ = s.reduce(ChatInput::SessionUpdate(vec![idle]), time(0));
    assert!(s
        .turn_notes
        .values()
        .any(|x| x.contains("turn ended; no message reply observed")));
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "decision.requested".into(),
        },
        time(0),
    );
    let mut f = facts(false);
    f.waiting_decisions = 3;
    f.decisions_truncated = true;
    let _ = s.reduce(ChatInput::ThreadsLoaded(tree_token(&e), Ok(f)), time(0));
    assert_eq!(s.waiting_decisions, 3);
    assert!(s.decisions_truncated);
}

// Break: calling a turn reply-less despite an observed reply during that binding's turn.
#[test]
fn observed_reply_suppresses_missing_reply_note() {
    let mut s = ready(vec![]);
    let mut card = seat("alpha/chief");
    card.working = true;
    let _ = s.reduce(ChatInput::SessionUpdate(vec![card.clone()]), time(0));
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(21, "alpha/chief", "owner", "answer")],
                22,
                None,
            )),
        ),
        time(0),
    );
    card.working = false;
    let _ = s.reduce(ChatInput::SessionUpdate(vec![card]), time(0));
    assert!(s.turn_notes.is_empty());
}

// Break: replacing a live hold with the deliberately hold-free history row.
#[test]
fn head_refresh_preserves_live_hold_and_old_pages_cannot_regress_facts() {
    let mut held = row(10, "owner", "alpha/chief", "work");
    held.status = DeliveryStatus::Queued;
    held.delivery.phase = DeliveryPhase::Waiting;
    held.delivery.reason = Some("focused".into());
    let mut s = ready(vec![held.clone()]);
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let mut historical = held;
    historical.delivery.phase = DeliveryPhase::Queued;
    historical.delivery.reason = None;
    let _ = s.reduce(
        ChatInput::PageLoaded(page_token(&e), Ok(page(vec![historical.clone()], 22, None))),
        time(0),
    );
    assert!(delivery_label(s.row(&MessageId::new(10)).unwrap()).contains("held: focused"));
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 23,
            kind: "msg.delivered".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(10, "owner", "alpha/chief", "work")],
                23,
                None,
            )),
        ),
        time(0),
    );
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 24,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(page_token(&e), Ok(page(vec![historical], 22, None))),
        time(0),
    );
    assert_eq!(
        s.row(&MessageId::new(10)).unwrap().status,
        DeliveryStatus::Delivered
    );
}

// Break: dropping the remaining System/self pages merely because a previous cache exists.
#[test]
fn unrelated_older_page_is_skipped_even_with_a_loaded_pair() {
    let mut s = ready(vec![]);
    let mut e = vec![];
    s.open_thread(ThreadKey::Direct(Address::System), &mut e);
    let e = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(10, "system", "owner", "related")],
                20,
                Some("older"),
            )),
        ),
        time(0),
    );
    assert!(e.is_empty());
    let _ = plain(&mut s, 'g');
    let e = plain(&mut s, 'g');
    let e = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(9, "system", "alpha/chief", "unrelated")],
                20,
                Some("older-still"),
            )),
        ),
        time(0),
    );
    assert!(e.iter().any(|e|matches!(e,ChatEffect::LoadPage{query,..}if query.cursor.as_deref()==Some("older-still"))));
}

// Break: All auto-acks a message authored by Owner to Owner.
#[test]
fn self_addressed_owner_rows_are_not_auto_acked() {
    let mut s = ready(vec![]);
    let mut e = vec![];
    s.open_thread(ThreadKey::All, &mut e);
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![
                    row(1, "owner", "owner", "self"),
                    row(2, "alpha/chief", "owner", "incoming"),
                ],
                30,
                None,
            )),
        ),
        time(0),
    );
    let e = present(&mut s, vec![MessageId::new(1), MessageId::new(2)], true);
    let ids = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::Ack { params, .. } = e {
                Some(params.ids.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(ids, vec![MessageId::new(2)]);
}

// Break: a stalled inbox read overwrites a later delivered/read history fact.
#[test]
fn late_progress_cannot_regress_delivered_and_read_facts() {
    let mut old = row(9, "owner", "alpha/chief", "work");
    old.status = DeliveryStatus::Queued;
    old.delivery.phase = DeliveryPhase::Queued;
    let mut s = ready(vec![old.clone()]);
    let _ = present(&mut s, vec![MessageId::new(9)], true);
    let e = s.reduce(ChatInput::Tick, time(1));
    let progress = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::LoadInbox { token, params } = e {
                params.id.map(|_| *token)
            } else {
                None
            }
        })
        .unwrap();
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "msg.read".into(),
        },
        time(1),
    );
    let mut delivered = row(9, "owner", "alpha/chief", "work");
    delivered.read_at = Some(time(1).utc);
    let _ = s.reduce(
        ChatInput::PageLoaded(page_token(&e), Ok(page(vec![delivered], 22, None))),
        time(1),
    );
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            progress,
            Ok(InboxResult {
                messages: vec![old],
                unread_count: 0,
                next_after: None,
            }),
        ),
        time(1),
    );
    let got = s.row(&MessageId::new(9)).unwrap();
    assert_eq!(got.status, DeliveryStatus::Delivered);
    assert_eq!(got.read_at, Some(time(1).utc));
    assert!(s
        .reduce(ChatInput::Tick, time(2))
        .iter()
        .all(|e| !matches!(e, ChatEffect::LoadInbox { .. })));
}

// Break: a late old success clears the draft belonging to a newer admitted send.
#[test]
fn late_unlocked_send_success_preserves_the_new_locked_draft() {
    let mut s = ready(vec![]);
    let e = enter_body(&mut s, "same body");
    let old = send_token(&e);
    let _ = s.reduce(ChatInput::SendDispatched(old), time(0));
    let _ = s.reduce(
        ChatInput::SendResult(old, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(0),
    );
    let _ = key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL, 10);
    let e = key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 10);
    let new = send_token(&e);
    assert_ne!(old, new);
    let e = s.reduce(
        ChatInput::SendResult(
            old,
            Ok(SendResult {
                message_id: MessageId::new(21),
            }),
        ),
        time(10),
    );
    assert!(e
        .iter()
        .any(|e| matches!(e,ChatEffect::RecordSend{token,..}if *token==old)));
    assert_eq!(s.view().draft, "same body");
    assert_eq!(s.mode, Mode::Insert);
    assert!(s.draft_locked());
}

// Break: n/N moves a thread/seat cursor instead of the current literal-search hit.
#[test]
fn search_hit_navigation_is_independent_of_focused_column() {
    let mut s = ready(vec![
        row(9, "alpha/chief", "owner", "hit"),
        row(10, "alpha/chief", "owner", "hit"),
    ]);
    s.column = Column::Threads;
    let _ = plain(&mut s, 'N');
    assert_eq!(s.view().selected, Some(MessageId::new(9)));
    assert_eq!(s.column, Column::Messages);
}

// Synthetic: independent review counterexamples retained as permanent regressions.
#[test]
fn review_unknown_send_cannot_match_a_row_predating_initial_tree() {
    let mut s = ChatState::new(ChatConfig {
        initial_thread: Some(address("alpha/chief")),
        caller_pane: Some("%8".into()),
    });
    let _ = connection(&mut s, true);
    let e = s.reduce(ChatInput::Start, time(0));
    let mut tree = facts(false);
    tree.tree_seq = 100;
    let _ = s.reduce(ChatInput::ThreadsLoaded(tree_token(&e), Ok(tree)), time(0));
    let e = enter_body(&mut s, "same body");
    let token = send_token(&e);
    let _ = s.reduce(ChatInput::SendDispatched(token), time(1));
    let recovery = s.reduce(
        ChatInput::SendResult(token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(1),
    );
    let e = s.reduce(
        ChatInput::PageLoaded(
            page_token(&recovery),
            Ok(page(
                vec![row(50, "owner", "alpha/chief", "same body")],
                100,
                None,
            )),
        ),
        time(1),
    );
    assert!(
        !e.iter().any(|e| matches!(e, ChatEffect::RecordSend { .. })),
        "m50 already existed at the initial seq100 tree, so it cannot prove this send committed"
    );
    assert!(s.draft_locked());
}

// Break: an inbox observation beyond the tree is omitted from the send boundary.
#[test]
fn unknown_send_boundary_includes_known_inbox_message_ids() {
    let mut s = ChatState::new(ChatConfig {
        initial_thread: Some(address("alpha/chief")),
        caller_pane: None,
    });
    let _ = connection(&mut s, true);
    let e = s.reduce(ChatInput::Start, time(0));
    let mut f = facts(false);
    f.tree_seq = 100;
    let e = s.reduce(ChatInput::ThreadsLoaded(tree_token(&e), Ok(f)), time(0));
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            summary_token(&e),
            Ok(InboxResult {
                messages: vec![row(150, "beta/chief", "owner", "already known")],
                unread_count: 1,
                next_after: None,
            }),
        ),
        time(0),
    );
    let e = enter_body(&mut s, "same body");
    let token = send_token(&e);
    let _ = s.reduce(ChatInput::SendDispatched(token), time(1));
    let e = s.reduce(
        ChatInput::SendResult(token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(1),
    );
    let e = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(149, "owner", "alpha/chief", "same body")],
                150,
                None,
            )),
        ),
        time(1),
    );
    assert!(
        !e.iter().any(|e| matches!(e, ChatEffect::RecordSend { .. })),
        "m149 is not newer than the already known m150"
    );
    assert!(s.draft_locked());
}

// Break: a known send result is omitted while its history/watch refresh is pending.
#[test]
fn unknown_send_boundary_includes_prior_acknowledged_send() {
    let mut s = ready(vec![]);
    let e = enter_body(&mut s, "same body");
    let first = send_token(&e);
    let _ = s.reduce(
        ChatInput::SendResult(
            first,
            Ok(SendResult {
                message_id: MessageId::new(100),
            }),
        ),
        time(0),
    );
    let e = enter_body(&mut s, "same body");
    let second = send_token(&e);
    let _ = s.reduce(ChatInput::SendDispatched(second), time(1));
    let e = s.reduce(
        ChatInput::SendResult(second, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(1),
    );
    let e = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(100, "owner", "alpha/chief", "same body")],
                100,
                None,
            )),
        ),
        time(1),
    );
    assert!(
        !e.iter().any(|e| matches!(e, ChatEffect::RecordSend { .. })),
        "m100 is the already acknowledged prior send"
    );
    assert!(s.draft_locked());
}

#[test]
fn review_dirty_tree_completion_does_not_restore_mutation_authority() {
    let mut s = ready(vec![]);
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "agent.ready".into(),
        },
        time(0),
    );
    let tree = tree_token(&e);
    let _ = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "agent.bound".into(),
        },
        time(0),
    );
    let mut old = facts(false);
    old.tree_seq = 21;
    let followup = s.reduce(ChatInput::ThreadsLoaded(tree, Ok(old)), time(0));
    assert!(followup
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadThreads { .. })));
    assert_eq!(
        mutations(&enter_body(&mut s, "must remain read only")),
        0,
        "a known dirty attribution result cannot restore Send authority"
    );
}

// Break: a dirty pre-event tree completion authorizes reads-as-seen before its follow-up.
#[test]
fn dirty_tree_keeps_ack_paused_until_its_post_event_followup() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "agent.ready".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "project.changed".into(),
        },
        time(0),
    );
    let mut old = facts(false);
    old.tree_seq = 21;
    let e = s.reduce(ChatInput::ThreadsLoaded(tree_token(&e), Ok(old)), time(0));
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        0
    );
    let mut fresh = facts(false);
    fresh.tree_seq = 22;
    let _ = s.reduce(ChatInput::ThreadsLoaded(tree_token(&e), Ok(fresh)), time(0));
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        1
    );
}

#[test]
fn review_late_history_cannot_regress_newer_progress_receipt() {
    let mut queued = row(9, "owner", "alpha/chief", "work");
    queued.status = DeliveryStatus::Queued;
    queued.delivery.phase = DeliveryPhase::Queued;
    let mut s = ready(vec![queued.clone()]);
    let _ = present(&mut s, vec![MessageId::new(9)], true);
    let head = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let progress = s.reduce(ChatInput::Tick, time(1));
    let token = progress
        .iter()
        .find_map(|e| match e {
            ChatEffect::LoadInbox { token, params } if params.id == Some(MessageId::new(9)) => {
                Some(*token)
            }
            _ => None,
        })
        .unwrap();
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            token,
            Ok(InboxResult {
                messages: vec![row(9, "owner", "alpha/chief", "work")],
                unread_count: 0,
                next_after: None,
            }),
        ),
        time(1),
    );
    assert_eq!(
        s.row(&MessageId::new(9)).unwrap().status,
        DeliveryStatus::Delivered
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(page_token(&head), Ok(page(vec![queued], 21, None))),
        time(1),
    );
    assert_eq!(
        s.row(&MessageId::new(9)).unwrap().status,
        DeliveryStatus::Delivered,
        "a history response captured before the receipt must not restore queued"
    );
}

// Break: either response order can undo terminal receipt/failure or a newer attempt.
#[test]
fn delivery_observations_never_regress_terminal_or_attempt_facts() {
    for (status, phase, attempt) in [
        (DeliveryStatus::Delivered, DeliveryPhase::Delivered, 1),
        (DeliveryStatus::Failed, DeliveryPhase::Failed, 1),
        (DeliveryStatus::Delivering, DeliveryPhase::Delivering, 2),
    ] {
        for progress_first in [true, false] {
            let mut queued = row(9, "owner", "alpha/chief", "work");
            queued.status = DeliveryStatus::Queued;
            queued.delivery.phase = DeliveryPhase::Queued;
            let mut s = ready(vec![queued.clone()]);
            let _ = present(&mut s, vec![MessageId::new(9)], true);
            let head = s.reduce(
                ChatInput::WatchEvent {
                    seq: 21,
                    kind: "msg.sent".into(),
                },
                time(0),
            );
            let progress = s.reduce(ChatInput::Tick, time(1));
            let token = progress
                .iter()
                .find_map(|e| match e {
                    ChatEffect::LoadInbox { token, params }
                        if params.id == Some(MessageId::new(9)) =>
                    {
                        Some(*token)
                    }
                    _ => None,
                })
                .unwrap();
            let mut fresh = queued.clone();
            fresh.status = status;
            fresh.delivery.phase = phase;
            fresh.delivery.attempt = attempt;
            if status == DeliveryStatus::Delivered {
                fresh.read_at = Some(time(1).utc);
            }
            let (history, inbox) = if progress_first {
                (queued, fresh)
            } else {
                (fresh, queued)
            };
            if progress_first {
                let _ = s.reduce(
                    ChatInput::InboxLoaded(
                        token,
                        Ok(InboxResult {
                            messages: vec![inbox],
                            unread_count: 0,
                            next_after: None,
                        }),
                    ),
                    time(1),
                );
                let _ = s.reduce(
                    ChatInput::PageLoaded(page_token(&head), Ok(page(vec![history], 21, None))),
                    time(1),
                );
            } else {
                let _ = s.reduce(
                    ChatInput::PageLoaded(page_token(&head), Ok(page(vec![history], 21, None))),
                    time(1),
                );
                let _ = s.reduce(
                    ChatInput::InboxLoaded(
                        token,
                        Ok(InboxResult {
                            messages: vec![inbox],
                            unread_count: 0,
                            next_after: None,
                        }),
                    ),
                    time(1),
                );
            }
            let got = s.row(&MessageId::new(9)).unwrap();
            assert_eq!(got.status, status, "progress_first={progress_first}");
            assert_eq!(got.delivery.attempt, attempt);
            if status == DeliveryStatus::Delivered {
                assert_eq!(got.read_at, Some(time(1).utc));
            }
        }
    }
}

#[test]
fn review_filtered_head_gap_resets_the_cached_window() {
    let mut s = ready(vec![]);
    let mut e = vec![];
    s.open_thread(ThreadKey::Direct(Address::System), &mut e);
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(600, "system", "owner", "cached")],
                600,
                Some("old-below-600"),
            )),
        ),
        time(0),
    );
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 2000,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let unrelated = (1101..=1200)
        .rev()
        .map(|id| row(id, "system", "alpha/chief", "not this pair"))
        .collect();
    let e = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(unrelated, 2000, Some("new-below-1101"))),
        ),
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(1100, "system", "owner", "new pair row")],
                2000,
                Some("new-below-1100"),
            )),
        ),
        time(0),
    );
    assert!(
        s.row(&MessageId::new(600)).is_none(),
        "do not silently join m600 to m1100 with an unqueried gap"
    );
    assert!(s.status.contains("history reloaded"));
}

// Break: a non-final failure is newer than Delivering at that same attempt number.
#[test]
fn nonfinal_failure_cannot_be_undone_by_same_attempt_delivery() {
    for progress_first in [true, false] {
        let mut delivering = row(9, "owner", "alpha/chief", "work");
        delivering.status = DeliveryStatus::Delivering;
        delivering.delivery.phase = DeliveryPhase::Delivering;
        let mut s = ready(vec![delivering.clone()]);
        let _ = present(&mut s, vec![MessageId::new(9)], true);
        let head = s.reduce(
            ChatInput::WatchEvent {
                seq: 21,
                kind: "msg.sent".into(),
            },
            time(0),
        );
        let e = s.reduce(ChatInput::Tick, time(1));
        let token = e
            .iter()
            .find_map(|e| match e {
                ChatEffect::LoadInbox { token, params } if params.id == Some(MessageId::new(9)) => {
                    Some(*token)
                }
                _ => None,
            })
            .unwrap();
        let mut queued = delivering.clone();
        queued.status = DeliveryStatus::Queued;
        queued.delivery.phase = DeliveryPhase::Queued;
        queued.delivery.reason = Some("retry after failed attempt".into());
        let (history, inbox) = if progress_first {
            (delivering, queued)
        } else {
            (queued, delivering)
        };
        if progress_first {
            let _ = s.reduce(
                ChatInput::InboxLoaded(
                    token,
                    Ok(InboxResult {
                        messages: vec![inbox],
                        unread_count: 0,
                        next_after: None,
                    }),
                ),
                time(1),
            );
            let _ = s.reduce(
                ChatInput::PageLoaded(page_token(&head), Ok(page(vec![history], 21, None))),
                time(1),
            );
        } else {
            let _ = s.reduce(
                ChatInput::PageLoaded(page_token(&head), Ok(page(vec![history], 22, None))),
                time(1),
            );
            let _ = s.reduce(
                ChatInput::InboxLoaded(
                    token,
                    Ok(InboxResult {
                        messages: vec![inbox],
                        unread_count: 0,
                        next_after: None,
                    }),
                ),
                time(1),
            );
        }
        let got = s.row(&MessageId::new(9)).unwrap();
        assert_eq!(
            got.status,
            DeliveryStatus::Queued,
            "progress_first={progress_first}"
        );
        assert_eq!(
            got.delivery.reason.as_deref(),
            Some("retry after failed attempt")
        );
    }
}

// Break: pair filtering hides a raw head's gap until a later matching page.
#[test]
fn filtered_raw_head_gap_is_visible_before_continuation() {
    let mut s = ready(vec![]);
    let mut e = vec![];
    s.open_thread(ThreadKey::Direct(Address::System), &mut e);
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(600, "system", "owner", "cached")],
                600,
                Some("old"),
            )),
        ),
        time(0),
    );
    let e = s.reduce(
        ChatInput::WatchEvent {
            seq: 2000,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let e = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(1200, "system", "alpha/chief", "not this pair")],
                2000,
                Some("new-below-1200"),
            )),
        ),
        time(0),
    );
    assert!(s.row(&MessageId::new(600)).is_none());
    assert!(s.status.contains("history reloaded"));
    assert_eq!(s.view().older.as_deref(), Some("new-below-1200"));
    assert!(e.iter().any(|e|matches!(e,ChatEffect::LoadPage{query,..} if query.cursor.as_deref()==Some("new-below-1200"))));
}

#[test]
fn review_cached_scopes_share_canonical_read_facts() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let mut e = vec![];
    s.open_thread(ThreadKey::All, &mut e);
    let mut read = row(9, "alpha/chief", "owner", "reply");
    read.read_at = Some(time(1).utc);
    let _ = s.reduce(
        ChatInput::PageLoaded(page_token(&e), Ok(page(vec![read], 21, None))),
        time(1),
    );
    assert!(s.row(&MessageId::new(9)).unwrap().read_at.is_some());
    s.open_thread(ThreadKey::Direct(address("alpha/chief")), &mut vec![]);
    assert!(
        s.row(&MessageId::new(9)).unwrap().read_at.is_some(),
        "switching scopes cannot regress a canonical read already observed for the same id"
    );
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        0
    );
}

// Break: inbox observations update only one private scope copy of a shared ID.
#[test]
fn canonical_receipts_reach_cached_views_without_injecting_unrelated_ids() {
    let mut queued = row(9, "owner", "alpha/chief", "work");
    queued.status = DeliveryStatus::Queued;
    queued.delivery.phase = DeliveryPhase::Queued;
    let mut s = ready(vec![queued.clone()]);
    let mut e = vec![];
    s.open_thread(ThreadKey::All, &mut e);
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![queued, row(10, "beta/chief", "owner", "other")],
                20,
                None,
            )),
        ),
        time(0),
    );
    s.open_thread(ThreadKey::Direct(address("alpha/chief")), &mut vec![]);
    let _ = present(&mut s, vec![MessageId::new(9)], true);
    let e = s.reduce(ChatInput::Tick, time(1));
    let token = e
        .iter()
        .find_map(|e| match e {
            ChatEffect::LoadInbox { token, params } if params.id == Some(MessageId::new(9)) => {
                Some(*token)
            }
            _ => None,
        })
        .unwrap();
    let mut delivered = row(9, "owner", "alpha/chief", "work");
    delivered.read_at = Some(time(1).utc);
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            token,
            Ok(InboxResult {
                messages: vec![delivered],
                unread_count: 0,
                next_after: None,
            }),
        ),
        time(1),
    );
    s.open_thread(ThreadKey::All, &mut vec![]);
    assert_eq!(
        s.row(&MessageId::new(9)).unwrap().status,
        DeliveryStatus::Delivered
    );
    assert_eq!(
        s.row(&MessageId::new(9)).unwrap().read_at,
        Some(time(1).utc)
    );
    assert!(s.row(&MessageId::new(10)).is_some());
    s.open_thread(ThreadKey::Direct(address("alpha/chief")), &mut vec![]);
    assert!(s.row(&MessageId::new(10)).is_none());
}

// Break: restoring an old view re-admits a history result below another view's head.
#[test]
fn canonical_view_head_cannot_regress_on_cached_scope_completion() {
    let mut queued = row(9, "owner", "alpha/chief", "work");
    queued.status = DeliveryStatus::Queued;
    queued.delivery.phase = DeliveryPhase::Queued;
    let mut s = ready(vec![queued.clone()]);
    let mut e = vec![];
    s.open_thread(ThreadKey::All, &mut e);
    let mut delivering = queued.clone();
    delivering.status = DeliveryStatus::Delivering;
    delivering.delivery.phase = DeliveryPhase::Delivering;
    delivering.delivery.attempt = 2; // A new attempt must advance beyond the queued prior attempt.
    let _ = s.reduce(
        ChatInput::PageLoaded(page_token(&e), Ok(page(vec![delivering], 30, None))),
        time(0),
    );
    let mut e = vec![];
    s.open_thread(ThreadKey::Direct(address("alpha/chief")), &mut e);
    assert_eq!(
        s.row(&MessageId::new(9)).unwrap().status,
        DeliveryStatus::Delivering
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(page_token(&e), Ok(page(vec![queued], 21, None))),
        time(1),
    );
    assert_eq!(
        s.row(&MessageId::new(9)).unwrap().status,
        DeliveryStatus::Delivering
    );
}

#[test]
fn review_unknown_ack_reconciles_fresh_unread_rows() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let ack = present(&mut s, vec![MessageId::new(9)], true);
    let token = ack
        .iter()
        .find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let e = s.reduce(
        ChatInput::AckResult(
            token,
            Err(ChatFailure::OutcomeUnknown("not yet known".into())),
        ),
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&e),
            Ok(page(
                vec![row(9, "alpha/chief", "owner", "reply")],
                22,
                None,
            )),
        ),
        time(1),
    );
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            summary_token(&e),
            Ok(InboxResult {
                messages: vec![row(9, "alpha/chief", "owner", "reply")],
                unread_count: 1,
                next_after: None,
            }),
        ),
        time(1),
    );
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        1,
        "post-unknown canonical unread reconciliation should not suppress this id forever"
    );
}

// Break: pre-outcome reads or later completions release suppression around an active retry.
#[test]
fn uncertain_ack_needs_a_post_outcome_read_and_suppresses_an_active_retry() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let old_head = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let ack = present(&mut s, vec![MessageId::new(9)], true);
    let ack_token = ack
        .iter()
        .find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let recovery = s.reduce(
        ChatInput::AckResult(ack_token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(0),
    );
    let followup = s.reduce(
        ChatInput::PageLoaded(
            page_token(&old_head),
            Ok(page(
                vec![row(9, "alpha/chief", "owner", "reply")],
                22,
                None,
            )),
        ),
        time(1),
    );
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        0,
        "a pre-outcome read is not reconciliation"
    );
    let by_id = recovery.iter().find_map(|e| match e {
        ChatEffect::LoadInbox { token, params } if params.id == Some(MessageId::new(9)) => {
            Some(*token)
        }
        _ => None,
    });
    assert!(
        by_id.is_some(),
        "unknown ack must refresh its exact canonical row"
    );
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            by_id.unwrap(),
            Ok(InboxResult {
                messages: vec![row(9, "alpha/chief", "owner", "reply")],
                unread_count: 1,
                next_after: None,
            }),
        ),
        time(1),
    );
    assert!(s.row(&MessageId::new(9)).unwrap().read_at.is_none());
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        1
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&followup),
            Ok(page(
                vec![row(9, "alpha/chief", "owner", "reply")],
                23,
                None,
            )),
        ),
        time(1),
    );
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        0,
        "a retry is already in flight"
    );
}

// Break: page-one summary/head bounds prevent an old visible row's ack from ever recovering.
#[test]
fn uncertain_ack_reconciles_by_id_outside_the_current_scope_and_first_page() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let ack = present(&mut s, vec![MessageId::new(9)], true);
    let ack_token = ack
        .iter()
        .find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let mut head = vec![];
    s.open_thread(ThreadKey::All, &mut head);
    let recovery = s.reduce(
        ChatInput::AckResult(ack_token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(0),
    );
    let rows: Vec<_> = (401..=500)
        .map(|id| row(id, "beta/chief", "owner", "newer unread"))
        .collect();
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&head),
            Ok(page(rows.clone(), 501, Some("older-than-first-page"))),
        ),
        time(1),
    );
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            summary_token(&recovery),
            Ok(InboxResult {
                messages: rows,
                unread_count: 101,
                next_after: Some(MessageId::new(401)),
            }),
        ),
        time(1),
    );
    let by_id = recovery.iter().find_map(|e| match e {
        ChatEffect::LoadInbox { token, params }
            if params.id == Some(MessageId::new(9)) && params.recipient == Some(Address::Owner) =>
        {
            Some(*token)
        }
        _ => None,
    });
    assert!(
        by_id.is_some(),
        "reconcile Owner m9 directly, not only the bounded first page"
    );
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            by_id.unwrap(),
            Ok(InboxResult {
                messages: vec![row(9, "alpha/chief", "owner", "reply")],
                unread_count: 101,
                next_after: None,
            }),
        ),
        time(1),
    );
    s.open_thread(ThreadKey::Direct(address("alpha/chief")), &mut vec![]);
    assert!(s.row(&MessageId::new(9)).unwrap().read_at.is_none());
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        1
    );
}

// Break: a failed by-ID reconciliation permanently holds an uncertain ack after reconnect.
#[test]
fn uncertain_ack_retries_failed_row_reconciliation_on_reconnect() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let ack = present(&mut s, vec![MessageId::new(9)], true);
    let ack_token = ack
        .iter()
        .find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let e = s.reduce(
        ChatInput::AckResult(ack_token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(0),
    );
    let token = e
        .iter()
        .find_map(|e| match e {
            ChatEffect::LoadInbox { token, params } if params.id == Some(MessageId::new(9)) => {
                Some(*token)
            }
            _ => None,
        })
        .unwrap();
    let _ = s.reduce(
        ChatInput::InboxLoaded(token, Err(ChatFailure::Unavailable("disconnected".into()))),
        time(1),
    );
    let _ = connection(&mut s, false);
    let e = connection(&mut s, true);
    assert!(!e.iter().any(|e| matches!(e, ChatEffect::Watch { .. })));
    let retry = e.iter().find_map(|e| match e {
        ChatEffect::LoadInbox { token, params } if params.id == Some(MessageId::new(9)) => {
            Some(*token)
        }
        _ => None,
    });
    assert!(
        retry.is_some(),
        "reconnect must re-read unresolved acknowledgments by ID"
    );
    let _ = s.reduce(
        ChatInput::InboxLoaded(
            retry.unwrap(),
            Ok(InboxResult {
                messages: vec![row(9, "alpha/chief", "owner", "reply")],
                unread_count: 1,
                next_after: None,
            }),
        ),
        time(1),
    );
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&e), Ok(facts(false))),
        time(1),
    );
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        1
    );
}

// Break: a no-send precheck refusal permanently suppressing an actually unread row.
#[test]
fn refused_ack_can_be_presented_again_without_inventing_read() {
    let mut s = ready(vec![row(1, "alpha/chief", "owner", "hello")]);
    let e = present(&mut s, vec![MessageId::new(1)], true);
    let token = e
        .iter()
        .find_map(|e| {
            if let ChatEffect::Ack { token, .. } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap();
    let reads = s.reduce(
        ChatInput::AckResult(
            token,
            Err(ChatFailure::Refused("not sent: tree read failed".into())),
        ),
        time(0),
    );
    assert!(s.row(&MessageId::new(1)).unwrap().read_at.is_none());
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(1)], true)),
        0
    );
    let effects = s.reduce(
        ChatInput::PageLoaded(
            page_token(&reads),
            Ok(page(
                vec![row(1, "alpha/chief", "owner", "hello")],
                21,
                None,
            )),
        ),
        time(1),
    );
    assert_eq!(mutations(&effects), 0);
    assert!(s.row(&MessageId::new(1)).unwrap().read_at.is_none());
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(1)], true, 1)),
        0
    );
    let fresh = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&reads), Ok(facts(false))),
        time(1),
    );
    assert_eq!(mutations(&fresh), 0);
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(1)], true, 9)),
        0
    );
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(1)], true, 10)),
        1
    );
}

// Synthetic: independent delta checks, no external resources or wall clocks.
#[test]
fn genuinely_new_send_recovers_before_initial_history_completes() {
    let mut s = ChatState::new(ChatConfig {
        initial_thread: Some(address("alpha/chief")),
        caller_pane: Some("%8".into()),
    });
    let _ = connection(&mut s, true);
    let start = s.reduce(ChatInput::Start, time(0));
    let mut tree = facts(false);
    tree.tree_seq = 100;
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&start), Ok(tree)),
        time(0),
    );
    let send = enter_body(&mut s, "new submission");
    let token = send_token(&send);
    let _ = s.reduce(ChatInput::SendDispatched(token), time(1));
    let recovery = s.reduce(
        ChatInput::SendResult(token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(1),
    );
    let effects = s.reduce(
        ChatInput::PageLoaded(
            page_token(&recovery),
            Ok(page(
                vec![row(101, "owner", "alpha/chief", "new submission")],
                101,
                None,
            )),
        ),
        time(1),
    );
    assert_eq!(effects.iter().filter(|e| matches!(e, ChatEffect::RecordSend { token: t, message_id, word_count: 2 } if *t == token && *message_id == MessageId::new(101))).count(), 1);
    assert!(!s.draft_locked());
    assert!(s.view().draft.is_empty());
}

#[test]
fn head_gap_discards_an_older_cursor_queued_before_the_reset() {
    let mut s = ready(vec![]);
    let initial = s.reduce(
        ChatInput::WatchEvent {
            seq: 601,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&initial),
            Ok(page(
                vec![row(600, "alpha/chief", "owner", "cached")],
                601,
                Some("old-below-600"),
            )),
        ),
        time(0),
    );
    let fresh = s.reduce(
        ChatInput::WatchEvent {
            seq: 901,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = plain(&mut s, 'g');
    let queued = plain(&mut s, 'g');
    assert!(
        queued.is_empty(),
        "the old-cursor request is coalesced behind the active head read"
    );
    let after_reset = s.reduce(
        ChatInput::PageLoaded(
            page_token(&fresh),
            Ok(page(
                vec![
                    row(900, "alpha/chief", "owner", "new"),
                    row(801, "alpha/chief", "owner", "new low"),
                ],
                901,
                Some("new-below-801"),
            )),
        ),
        time(0),
    );
    assert!(s.status.contains("history reloaded"));
    let old_cursor = after_reset.iter().find_map(|effect| match effect {
        ChatEffect::LoadPage { token, query }
            if query.cursor.as_deref() == Some("old-below-600") =>
        {
            Some(*token)
        }
        _ => None,
    });
    if let Some(token) = old_cursor {
        let _ = s.reduce(
            ChatInput::PageLoaded(
                token,
                Ok(ChatPage {
                    rows: vec![row(599, "alpha/chief", "owner", "old tail")],
                    head: 901,
                    fence: 601,
                    older: Some("old-below-599".into()),
                }),
            ),
            time(1),
        );
    }
    eprintln!(
        "after reset/follow-up: ids={:?}, fence={}, older={:?}",
        s.view().ids,
        s.view().fence,
        s.view().older
    );
    assert!(old_cursor.is_none(), "head-gap reset must not issue the old below-m600 cursor after switching to the new below-m801 fence");
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![801, 900],
        "no silent hole may be reintroduced by a queued old-fence continuation"
    );
    assert_eq!(s.view().older.as_deref(), Some("new-below-801"));
}

#[test]
fn uncertain_ack_reads_are_per_id_coalesced_and_terminate_after_a_finite_burst() {
    let mut s = ready(vec![
        row(9, "alpha/chief", "owner", "one"),
        row(10, "alpha/chief", "owner", "two"),
    ]);
    let ack = present(&mut s, vec![MessageId::new(9), MessageId::new(10)], true);
    let token = ack
        .iter()
        .find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let recovery = s.reduce(
        ChatInput::AckResult(token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(0),
    );
    let initial_reads: Vec<_> = recovery
        .iter()
        .filter_map(|e| match e {
            ChatEffect::LoadInbox { token, params } if params.id.is_some() => {
                assert_eq!(params.recipient, Some(Address::Owner));
                assert!(!params.unread_only);
                Some((*token, params.id.unwrap()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(initial_reads.len(), 2);
    for seq in 21..=40 {
        let burst = s.reduce(
            ChatInput::WatchEvent {
                seq,
                kind: "msg.sent".into(),
            },
            time(0),
        );
        assert!(!burst
            .iter()
            .any(|e| matches!(e, ChatEffect::LoadInbox { params, .. } if params.id.is_some())));
        assert_eq!(mutations(&burst), 0);
        for id in [MessageId::new(9), MessageId::new(10)] {
            assert_eq!(s.reads.values().filter(|pending| matches!(pending.purpose, ReadPurpose::Progress(current) if current == id)).count(), 1);
        }
    }
    for (read_token, id) in initial_reads {
        let name = if id == MessageId::new(9) {
            "one"
        } else {
            "two"
        };
        let followup = s.reduce(
            ChatInput::InboxLoaded(
                read_token,
                Ok(InboxResult {
                    messages: vec![row(id.seq(), "alpha/chief", "owner", name)],
                    unread_count: 2,
                    next_after: None,
                }),
            ),
            time(1),
        );
        let next: Vec<_> = followup
            .iter()
            .filter_map(|e| match e {
                ChatEffect::LoadInbox { token, params } if params.id == Some(id) => Some(*token),
                _ => None,
            })
            .collect();
        assert_eq!(
            next.len(),
            1,
            "one dirty follow-up, not one request per event"
        );
        let final_effects = s.reduce(
            ChatInput::InboxLoaded(
                *next.first().unwrap(),
                Ok(InboxResult {
                    messages: vec![row(id.seq(), "alpha/chief", "owner", name)],
                    unread_count: 2,
                    next_after: None,
                }),
            ),
            time(1),
        );
        assert!(!final_effects
            .iter()
            .any(|e| matches!(e, ChatEffect::LoadInbox { params, .. } if params.id == Some(id))));
    }
    assert!(s.unknown_acks.is_empty());
}

// Break: dropping all older paging instead of discarding only the old window's continuation.
#[test]
fn older_paging_after_a_head_gap_uses_the_new_cursor_and_fence() {
    let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
    let fresh = s.reduce(
        ChatInput::WatchEvent {
            seq: 901,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let reset = s.reduce(
        ChatInput::PageLoaded(
            page_token(&fresh),
            Ok(page(
                vec![
                    row(900, "alpha/chief", "owner", "new"),
                    row(801, "alpha/chief", "owner", "new low"),
                ],
                901,
                Some("new-below-801"),
            )),
        ),
        time(0),
    );
    assert!(reset.is_empty());
    assert_eq!(s.view().fence, 901);
    let _ = plain(&mut s, 'g');
    let older = plain(&mut s, 'g');
    assert!(older
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadPage { query, .. }
        if query.cursor.as_deref() == Some("new-below-801"))));
    let loaded = s.reduce(
        ChatInput::PageLoaded(
            page_token(&older),
            Ok(ChatPage {
                rows: vec![row(800, "alpha/chief", "owner", "new older")],
                head: 950,
                fence: 901,
                older: Some("new-below-800".into()),
            }),
        ),
        time(1),
    );
    assert!(loaded.is_empty());
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![800, 801, 900]
    );
    assert_eq!(s.view().fence, 901);
    assert_eq!(s.view().older.as_deref(), Some("new-below-800"));
    let generation = s.generation;
    assert!(s
        .reduce(
            ChatInput::PageLoaded(
                page_token(&fresh),
                Ok(page(
                    vec![row(599, "alpha/chief", "owner", "obsolete replay")],
                    601,
                    Some("old-below-599"),
                ))
            ),
            time(1)
        )
        .is_empty());
    assert_eq!(s.generation, generation);
    assert_eq!(s.view().fence, 901);
}

// Break: an old queued cursor survives indirectly as a filtered head continuation's follow-up.
#[test]
fn filtered_head_gap_discards_the_pre_reset_older_followup() {
    let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
    s.view_mut().older = Some("old-below-600".into());
    let fresh = s.reduce(
        ChatInput::WatchEvent {
            seq: 901,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = plain(&mut s, 'g');
    assert!(plain(&mut s, 'g').is_empty());
    let auto = s.reduce(
        ChatInput::PageLoaded(
            page_token(&fresh),
            Ok(page(
                vec![row(900, "beta/chief", "owner", "not this pair")],
                901,
                Some("new-below-900"),
            )),
        ),
        time(0),
    );
    assert!(s.status.contains("history reloaded"));
    assert!(s.view().ids.is_empty());
    assert!(auto
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadPage { query, .. }
        if query.cursor.as_deref() == Some("new-below-900"))));
    let after = s.reduce(
        ChatInput::PageLoaded(
            page_token(&auto),
            Ok(page(
                vec![row(801, "alpha/chief", "owner", "new low")],
                901,
                Some("new-below-801"),
            )),
        ),
        time(1),
    );
    assert!(
        after.is_empty(),
        "the filtered continuation must not revive an old-window request: {after:?}"
    );
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![801]
    );
    assert_eq!(s.view().fence, 901);
    assert_eq!(s.view().older.as_deref(), Some("new-below-801"));
}

// Break: invalidating older cursors also loses a legitimate coalesced fresh-head refresh.
#[test]
fn head_gap_preserves_one_dirty_head_refresh() {
    let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
    let fresh = s.reduce(
        ChatInput::WatchEvent {
            seq: 901,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    for seq in 902..=920 {
        assert!(s
            .reduce(
                ChatInput::WatchEvent {
                    seq,
                    kind: "msg.sent".into()
                },
                time(0)
            )
            .is_empty());
    }
    let after = s.reduce(
        ChatInput::PageLoaded(
            page_token(&fresh),
            Ok(page(
                vec![row(900, "alpha/chief", "owner", "new low")],
                901,
                Some("new-below-900"),
            )),
        ),
        time(0),
    );
    assert_eq!(
        after
            .iter()
            .filter(|e| matches!(e, ChatEffect::LoadPage { query, .. }
        if query.cursor.is_none()))
            .count(),
        1
    );
    let done = s.reduce(
        ChatInput::PageLoaded(
            page_token(&after),
            Ok(page(
                vec![
                    row(919, "alpha/chief", "owner", "latest"),
                    row(900, "alpha/chief", "owner", "new low"),
                ],
                920,
                Some("new-below-900"),
            )),
        ),
        time(1),
    );
    assert!(done.is_empty());
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![900, 919]
    );
    assert_eq!(s.view().fence, 920);
}

// Break: an older result or its queued continuation mutates a restarted search window.
#[test]
fn search_restart_rejects_an_inflight_older_page_and_its_old_cursor() {
    let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
    s.view_mut().older = Some("old-below-600".into());
    let _ = plain(&mut s, 'g');
    let old = plain(&mut s, 'g');
    let _ = plain(&mut s, 'g');
    assert!(plain(&mut s, 'g').is_empty());
    let _ = plain(&mut s, '/');
    for c in "needle".chars() {
        let _ = plain(&mut s, c);
    }
    assert!(key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 0).is_empty());
    let next = s.reduce(
        ChatInput::PageLoaded(
            page_token(&old),
            Ok(page(
                vec![row(
                    599,
                    "alpha/chief",
                    "owner",
                    "needle from discarded window",
                )],
                601,
                Some("old-below-599"),
            )),
        ),
        time(1),
    );
    assert!(s.view().ids.is_empty());
    assert!(s.view().older.is_none());
    assert_eq!(
        next.iter()
            .filter(|e| matches!(e, ChatEffect::LoadPage { query, .. }
        if query.cursor.is_none() && query.text.as_deref() == Some("needle")))
            .count(),
        1
    );
    let loaded = s.reduce(
        ChatInput::PageLoaded(
            page_token(&next),
            Ok(page(
                vec![row(800, "alpha/chief", "owner", "needle from new head")],
                901,
                Some("new-below-800"),
            )),
        ),
        time(1),
    );
    assert!(loaded.is_empty());
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![800]
    );
    assert_eq!(s.view().fence, 901);
    assert_eq!(s.view().older.as_deref(), Some("new-below-800"));
    let generation = s.generation;
    assert!(s
        .reduce(
            ChatInput::PageLoaded(
                page_token(&old),
                Err(ChatFailure::Unavailable("old failure".into()))
            ),
            time(1)
        )
        .is_empty());
    assert_eq!(s.generation, generation);
}

// Break: a cached continuation is stamped with another active thread's window epoch.
#[test]
fn cached_older_continuation_keeps_its_own_window_epoch() {
    let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
    let fresh = s.reduce(
        ChatInput::WatchEvent {
            seq: 901,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::PageLoaded(
            page_token(&fresh),
            Ok(page(
                vec![
                    row(900, "alpha/chief", "owner", "new"),
                    row(801, "alpha/chief", "owner", "new low"),
                ],
                901,
                Some("new-below-801"),
            )),
        ),
        time(0),
    );
    let _ = plain(&mut s, 'g');
    let older = plain(&mut s, 'g');
    let _ = plain(&mut s, 'g');
    assert!(plain(&mut s, 'g').is_empty());
    s.open_thread(ThreadKey::Direct(address("beta/chief")), &mut vec![]);
    let continued = s.reduce(
        ChatInput::PageLoaded(
            page_token(&older),
            Ok(page(
                vec![row(800, "alpha/chief", "owner", "older")],
                901,
                Some("new-below-800"),
            )),
        ),
        time(1),
    );
    assert!(continued
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadPage { query, .. }
        if query.cursor.as_deref() == Some("new-below-801"))));
    s.open_thread(ThreadKey::Direct(address("alpha/chief")), &mut vec![]);
    let refreshed = s.reduce(
        ChatInput::PageLoaded(
            page_token(&continued),
            Ok(page(
                vec![row(800, "alpha/chief", "owner", "older")],
                901,
                Some("new-below-800"),
            )),
        ),
        time(1),
    );
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![800, 801, 900]
    );
    assert_eq!(s.view().fence, 901);
    assert_eq!(s.view().older.as_deref(), Some("new-below-800"));
    assert_eq!(
        refreshed
            .iter()
            .filter(|e| matches!(e, ChatEffect::LoadPage { query, .. }
        if query.cursor.is_none()))
            .count(),
        1
    );
}

// Break: the shared motion parser's Escape/Q quit loses in-memory drafts.
#[test]
fn normal_escape_and_upper_q_keep_drafts_and_chat_open() {
    for normal_key in [KeyCode::Esc, KeyCode::Char('Q')] {
        let mut s = ready(vec![]);
        let _ = plain(&mut s, 'i');
        let _ = plain(&mut s, 'x');
        assert!(key(&mut s, KeyCode::Esc, KeyModifiers::NONE, 0).is_empty());
        assert_eq!(s.mode, Mode::Normal);
        assert!(key(&mut s, normal_key, KeyModifiers::NONE, 0).is_empty());
        assert!(!s.closed);
        assert_eq!(s.view().draft, "x");
        let _ = plain(&mut s, '/');
        let _ = plain(&mut s, 'z');
        let _ = key(&mut s, KeyCode::Esc, KeyModifiers::NONE, 0);
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.view().search.is_empty());
        let _ = plain(&mut s, 'i');
        let _ = plain(&mut s, 'q');
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.view().draft, "xq");
        assert!(!s.closed);
    }
}

#[test]
fn normal_quit_keys_still_close_chat() {
    for (code, modifiers) in [
        (KeyCode::Char('q'), KeyModifiers::NONE),
        (KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut s = ready(vec![]);
        let effects = key(&mut s, code, modifiers, 0);
        assert!(s.closed);
        assert!(effects
            .iter()
            .any(|e| matches!(e, ChatEffect::Quit { error: None })));
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, ChatEffect::Unwatch))
                .count(),
            1
        );
    }
}

// Break: refusal releases suppression on a redraw or on a read started before refusal.
#[test]
fn refused_ack_waits_for_a_post_refusal_read_before_retry() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let old = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let ack = present(&mut s, vec![MessageId::new(9)], true);
    let token = ack
        .iter()
        .find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let refused = s.reduce(
        ChatInput::AckResult(token, Err(ChatFailure::Refused("tree unavailable".into()))),
        time(0),
    );
    assert_eq!(mutations(&refused), 0);
    for _ in 0..20 {
        assert_eq!(
            mutations(&present(&mut s, vec![MessageId::new(9)], true)),
            0,
            "redraw cannot retry a refused Ack"
        );
    }
    let fresh = s.reduce(
        ChatInput::PageLoaded(
            page_token(&old),
            Ok(page(
                vec![row(9, "alpha/chief", "owner", "reply")],
                21,
                None,
            )),
        ),
        time(1),
    );
    assert_eq!(mutations(&fresh), 0);
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        0,
        "a pre-refusal read is not fresh evidence"
    );
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&refused), Ok(facts(false))),
        time(10),
    );
    let _ = s.reduce(ChatInput::Tick, time(10));
    assert_eq!(mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)), 0,
        "the old message read still cannot release suppression even after attribution and time recover");
    let loaded = s.reduce(
        ChatInput::PageLoaded(
            page_token(&fresh),
            Ok(page(
                vec![row(9, "alpha/chief", "owner", "reply")],
                22,
                None,
            )),
        ),
        time(10),
    );
    assert_eq!(mutations(&loaded), 0);
    let retry = present_at(&mut s, vec![MessageId::new(9)], true, 10);
    assert_eq!(mutations(&retry), 1);
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)),
        0
    );
    let retry_token = retry
        .iter()
        .find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let reads = s.reduce(
        ChatInput::AckResult(
            retry_token,
            Err(ChatFailure::Refused("still unavailable".into())),
        ),
        time(10),
    );
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)),
        0
    );
    let read_token = reads
        .iter()
        .find_map(|e| match e {
            ChatEffect::LoadPage { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    let mut seen = row(9, "alpha/chief", "owner", "reply");
    seen.read_at = Some(time(11).utc);
    let effects = s.reduce(
        ChatInput::PageLoaded(read_token, Ok(page(vec![seen], 23, None))),
        time(11),
    );
    assert_eq!(mutations(&effects), 0);
    assert_eq!(
        s.row(&MessageId::new(9)).unwrap().read_at,
        Some(time(11).utc)
    );
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 11)),
        0
    );
}

// Break: older navigation overwrites a dirty head read, or head priority loses older intent.
#[test]
fn queued_head_refresh_runs_before_older_paging_in_either_request_order() {
    for head_first in [true, false] {
        let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
        s.view_mut().older = Some("old-below-600".into());
        let active = s.reduce(
            ChatInput::WatchEvent {
                seq: 901,
                kind: "msg.sent".into(),
            },
            time(0),
        );
        for seq in 902..=920 {
            if head_first {
                assert!(s
                    .reduce(
                        ChatInput::WatchEvent {
                            seq,
                            kind: "msg.sent".into()
                        },
                        time(0)
                    )
                    .is_empty());
            }
            let _ = plain(&mut s, 'g');
            assert!(plain(&mut s, 'g').is_empty());
            if !head_first {
                assert!(s
                    .reduce(
                        ChatInput::WatchEvent {
                            seq,
                            kind: "msg.sent".into()
                        },
                        time(0)
                    )
                    .is_empty());
            }
        }
        let head = s.reduce(
            ChatInput::PageLoaded(
                page_token(&active),
                Ok(page(
                    vec![
                        row(900, "alpha/chief", "owner", "new"),
                        row(600, "alpha/chief", "owner", "cached"),
                    ],
                    920,
                    Some("head-below-600"),
                )),
            ),
            time(1),
        );
        assert!(
            matches!(head.as_slice(), [ChatEffect::LoadPage { query, .. }] if query.cursor.is_none()),
            "head_first={head_first}: {head:?}"
        );
        let older = s.reduce(
            ChatInput::PageLoaded(
                page_token(&head),
                Ok(page(
                    vec![
                        row(901, "alpha/chief", "owner", "newest"),
                        row(900, "alpha/chief", "owner", "new"),
                        row(600, "alpha/chief", "owner", "cached"),
                    ],
                    921,
                    Some("head-below-600"),
                )),
            ),
            time(1),
        );
        assert!(
            matches!(older.as_slice(), [ChatEffect::LoadPage { query, .. }] if query.cursor.as_deref() == Some("old-below-600")),
            "head_first={head_first}: {older:?}"
        );
        let done = s.reduce(
            ChatInput::PageLoaded(
                page_token(&older),
                Ok(ChatPage {
                    rows: vec![row(599, "alpha/chief", "owner", "older")],
                    head: 921,
                    fence: 20,
                    older: Some("old-below-599".into()),
                }),
            ),
            time(1),
        );
        assert!(done.is_empty());
        assert_eq!(
            s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
            vec![599, 600, 900, 901]
        );
        assert_eq!(s.view().fence, 20);
    }
}

// Break: preserving a head refresh carries a discarded old cursor into the new window.
#[test]
fn head_priority_still_discards_older_work_after_a_gap() {
    let mut s = ready(vec![row(600, "alpha/chief", "owner", "cached")]);
    s.view_mut().older = Some("old-below-600".into());
    let active = s.reduce(
        ChatInput::WatchEvent {
            seq: 901,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = s.reduce(
        ChatInput::WatchEvent {
            seq: 902,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let _ = plain(&mut s, 'g');
    let _ = plain(&mut s, 'g');
    let head = s.reduce(
        ChatInput::PageLoaded(
            page_token(&active),
            Ok(page(
                vec![
                    row(900, "alpha/chief", "owner", "new"),
                    row(801, "alpha/chief", "owner", "new low"),
                ],
                901,
                Some("new-below-801"),
            )),
        ),
        time(1),
    );
    assert!(
        matches!(head.as_slice(), [ChatEffect::LoadPage { query, .. }] if query.cursor.is_none()),
        "{head:?}"
    );
    let done = s.reduce(
        ChatInput::PageLoaded(
            page_token(&head),
            Ok(page(
                vec![
                    row(901, "alpha/chief", "owner", "newest"),
                    row(900, "alpha/chief", "owner", "new"),
                    row(801, "alpha/chief", "owner", "new low"),
                ],
                902,
                Some("new-below-801"),
            )),
        ),
        time(1),
    );
    assert!(
        done.is_empty(),
        "no old cursor may reappear after the head refresh: {done:?}"
    );
    assert_eq!(
        s.view().ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        vec![801, 900, 901]
    );
    assert_eq!(s.view().fence, 902);
    assert_eq!(s.view().older.as_deref(), Some("new-below-801"));
}

// Break: delayed startup unread facts override the owner's navigation/editor intent.
#[test]
fn first_key_cancels_initial_choice_before_a_late_summary() {
    for failed_first in [false, true] {
        let mut s = ChatState::new(ChatConfig::default());
        let _ = connection(&mut s, true);
        let start = s.reduce(ChatInput::Start, time(0));
        let loaded = s.reduce(
            ChatInput::ThreadsLoaded(tree_token(&start), Ok(facts(false))),
            time(0),
        );
        let summary = if failed_first {
            let _ = s.reduce(
                ChatInput::InboxLoaded(
                    summary_token(&loaded),
                    Err(ChatFailure::Unavailable("slow summary".into())),
                ),
                time(0),
            );
            summary_token(&s.reduce(
                ChatInput::WatchEvent {
                    seq: 21,
                    kind: "msg.sent".into(),
                },
                time(0),
            ))
        } else {
            summary_token(&loaded)
        };
        let _ = plain(&mut s, 'h');
        let _ = plain(&mut s, 'j');
        let _ = plain(&mut s, 'j');
        let _ = key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 0);
        assert_eq!(s.thread, ThreadKey::Direct(address("beta/chief")));
        let _ = plain(&mut s, 'i');
        let _ = plain(&mut s, 'x');
        assert_eq!(s.mode, Mode::Insert);
        let effects = s.reduce(
            ChatInput::InboxLoaded(
                summary,
                Ok(InboxResult {
                    messages: vec![],
                    unread_count: 0,
                    next_after: None,
                }),
            ),
            time(1),
        );
        assert_eq!(mutations(&effects), 0);
        assert_eq!(
            s.thread,
            ThreadKey::Direct(address("beta/chief")),
            "failed_first={failed_first}"
        );
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.view().draft, "x");
        let _ = plain(&mut s, 'q');
        assert!(!s.closed);
        assert_eq!(s.view().draft, "xq");
        let send = key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 1);
        assert!(
            matches!(send.as_slice(), [ChatEffect::Send { params, .. }] if params.to == address("beta/chief") && params.body == "xq")
        );
    }
}

// Break: an unlocked old token claims the row already recorded for the resend.
#[test]
fn reconciliation_does_not_record_an_already_committed_message_twice() {
    let mut s = ready(vec![]);
    let first = send_token(&enter_body(&mut s, "same body"));
    let _ = s.reduce(ChatInput::SendDispatched(first), time(0));
    let pending = s.reduce(
        ChatInput::SendResult(first, Err(ChatFailure::OutcomeUnknown("lost".into()))),
        time(0),
    );
    let _ = key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL, 10);
    let resend = send_token(&key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 10));
    let accepted = s.reduce(
        ChatInput::SendResult(
            resend,
            Ok(SendResult {
                message_id: MessageId::new(21),
            }),
        ),
        time(10),
    );
    assert_eq!(accepted.iter().filter(|e| matches!(e, ChatEffect::RecordSend { message_id, .. } if *message_id == MessageId::new(21))).count(), 1);
    let old = s.reduce(
        ChatInput::PageLoaded(
            page_token(&pending),
            Ok(page(
                vec![row(21, "owner", "alpha/chief", "same body")],
                21,
                None,
            )),
        ),
        time(11),
    );
    assert!(
        !old.iter()
            .any(|e| matches!(e, ChatEffect::RecordSend { .. })),
        "one ID must not be recorded for both tokens: {old:?}"
    );
    assert!(s.has_unknown_send());
    let refresh = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "msg.sent".into(),
        },
        time(11),
    );
    let recovery = refresh
        .iter()
        .find_map(|e| match e {
            ChatEffect::LoadPage { token, .. }
                if s.reads.get(token).is_some_and(
                    |p| matches!(p.purpose, ReadPurpose::Reconcile { send, .. } if send == first),
                ) =>
            {
                Some(*token)
            }
            _ => None,
        })
        .unwrap();
    let new = s.reduce(
        ChatInput::PageLoaded(
            recovery,
            Ok(page(
                vec![row(22, "owner", "alpha/chief", "same body")],
                22,
                None,
            )),
        ),
        time(11),
    );
    assert_eq!(new.iter().filter(|e| matches!(e, ChatEffect::RecordSend { token, message_id, word_count: 2 } if *token == first && *message_id == MessageId::new(22))).count(), 1);
    assert!(!s.has_unknown_send());
    assert!(s
        .reduce(
            ChatInput::PageLoaded(
                recovery,
                Ok(page(
                    vec![row(22, "owner", "alpha/chief", "same body")],
                    22,
                    None
                ))
            ),
            time(11)
        )
        .is_empty());
}

// Break: late acceptance after explicit unlock turns the owner's Insert text into commands.
#[test]
fn late_unlocked_reconciliation_keeps_insert_and_the_new_draft() {
    for history in [true, false] {
        let mut s = ready(vec![]);
        let token = send_token(&enter_body(&mut s, "old body"));
        let _ = s.reduce(ChatInput::SendDispatched(token), time(0));
        let recovery = s.reduce(
            ChatInput::SendResult(token, Err(ChatFailure::OutcomeUnknown("lost".into()))),
            time(0),
        );
        let _ = key(&mut s, KeyCode::Char('r'), KeyModifiers::CONTROL, 10);
        let _ = plain(&mut s, 'x');
        assert_eq!(s.mode, Mode::Insert);
        let input = if history {
            ChatInput::PageLoaded(
                page_token(&recovery),
                Ok(page(
                    vec![row(21, "owner", "alpha/chief", "old body")],
                    21,
                    None,
                )),
            )
        } else {
            ChatInput::SendResult(
                token,
                Ok(SendResult {
                    message_id: MessageId::new(21),
                }),
            )
        };
        let effects = s.reduce(input, time(11));
        assert_eq!(effects.iter().filter(|e| matches!(e, ChatEffect::RecordSend { message_id, .. } if *message_id == MessageId::new(21))).count(), 1);
        assert_eq!(s.mode, Mode::Insert, "history={history}");
        assert_eq!(s.view().draft, "old bodyx");
        assert!(!s.draft_locked());
        let _ = plain(&mut s, 'q');
        assert_eq!(s.view().draft, "old bodyxq");
        assert!(!s.closed);
    }
}

// Break: a failed first tree read cannot recover, or ticks dirty a retry forever.
#[test]
fn failed_initial_tree_retries_on_tick_without_dirtying_the_retry() {
    for bound in [false, true] {
        let mut s = ChatState::new(ChatConfig {
            initial_thread: Some(address("alpha/chief")),
            caller_pane: None,
        });
        let _ = connection(&mut s, true);
        let start = s.reduce(ChatInput::Start, time(0));
        let _ = s.reduce(
            ChatInput::ThreadsLoaded(
                tree_token(&start),
                Err(ChatFailure::Unavailable("tree unavailable".into())),
            ),
            time(0),
        );
        let retry = s.reduce(ChatInput::Tick, time(1));
        assert!(
            matches!(retry.as_slice(), [ChatEffect::LoadThreads { .. }]),
            "{retry:?}"
        );
        for seconds in 2..20 {
            assert!(s.reduce(ChatInput::Tick, time(seconds)).is_empty());
        }
        let initialized = s.reduce(
            ChatInput::ThreadsLoaded(tree_token(&retry), Ok(facts(bound))),
            time(20),
        );
        if bound {
            assert!(matches!(
                initialized.as_slice(),
                [ChatEffect::Quit {
                    error: Some(ChatFailure::Refused(_))
                }]
            ));
            assert!(s.closed);
        } else {
            assert_eq!(
                initialized
                    .iter()
                    .filter(|e| matches!(e, ChatEffect::Watch { .. }))
                    .count(),
                1
            );
            assert!(s.initialized);
            assert!(s.tree_fresh);
            let _ = plain(&mut s, 'i');
            assert_eq!(s.mode, Mode::Insert);
        }
    }
}

// Break: Enter silently loses the owner's send intent while attribution is being checked.
#[test]
fn enter_during_tree_refresh_keeps_draft_and_explains_the_wait() {
    let mut s = ready(vec![]);
    let _ = plain(&mut s, 'i');
    for c in "keep this".chars() {
        let _ = plain(&mut s, c);
    }
    let tree = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "agent.updated".into(),
        },
        time(0),
    );
    for _ in 0..10 {
        let effects = key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 0);
        assert!(effects.is_empty());
        assert!(
            s.status.contains("checking who you are; try again"),
            "{}",
            s.status
        );
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.view().draft, "keep this");
        assert!(!s.draft_locked());
    }
    let fresh = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&tree), Ok(facts(false))),
        time(1),
    );
    assert_eq!(mutations(&fresh), 0);
    let sent = key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 1);
    assert!(
        matches!(sent.as_slice(), [ChatEffect::Send { params, .. }] if params.body == "keep this")
    );
    assert!(key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 1).is_empty());
}

// Break: rearming a failed Watch leaves the cache stale and attribution trusted indefinitely.
#[test]
fn watch_recovery_refreshes_stale_history_and_attribution() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "reply")]);
    let _ = s.reduce(
        ChatInput::Connection {
            online: true,
            methods: vec![],
            watch_available: false,
        },
        time(0),
    );
    assert!(s.view().stale);
    let recovered = connection(&mut s, true);
    assert!(
        recovered
            .iter()
            .any(|e| matches!(e, ChatEffect::LoadThreads { .. })),
        "recovery needs fresh attribution: {recovered:?}"
    );
    assert!(
        recovered
            .iter()
            .any(|e| matches!(e, ChatEffect::LoadPage { .. })),
        "recovery needs a fresh head: {recovered:?}"
    );
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        0
    );
    let loaded = s.reduce(
        ChatInput::PageLoaded(
            page_token(&recovered),
            Ok(page(
                vec![row(9, "alpha/chief", "owner", "reply")],
                21,
                None,
            )),
        ),
        time(1),
    );
    assert_eq!(mutations(&loaded), 0);
    assert!(!s.view().stale);
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        0
    );
    let facts_loaded = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&recovered), Ok(facts(false))),
        time(1),
    );
    assert_eq!(mutations(&facts_loaded), 0);
    assert_eq!(
        mutations(&present(&mut s, vec![MessageId::new(9)], true)),
        1
    );
    assert!(connection(&mut s, true).is_empty());
}

// Synthetic: "watch" is the approved controller-only capability marker, not an RPC method.
fn watch_connection(
    s: &mut ChatState,
    available: bool,
    marker: bool,
    seconds: u64,
) -> Vec<ChatEffect> {
    let mut methods: Vec<_> = [
        "org.tree",
        "msg.history",
        "msg.inbox",
        "msg.send",
        "msg.ack",
    ]
    .map(str::to_owned)
    .to_vec();
    if marker {
        methods.push("watch".into());
    }
    s.reduce(
        ChatInput::Connection {
            online: true,
            methods,
            watch_available: available,
        },
        time(seconds),
    )
}

// Break: a supported-but-stopped watch is mislabeled too old or authorizes stale mutations.
#[test]
fn stopped_watch_is_distinct_from_an_unsupported_daemon() {
    for supported in [false, true] {
        let mut s = ready(vec![row(9, "alpha/chief", "owner", "cached")]);
        let _ = plain(&mut s, 'i');
        let _ = plain(&mut s, 'x');
        assert!(watch_connection(&mut s, false, supported, 50).is_empty());
        let expected = if supported {
            "live updates stopped; retrying"
        } else {
            "daemon too old: chat methods/watch unavailable · read only"
        };
        assert_eq!(s.status, expected);
        assert_eq!(s.compatible, supported);
        assert!(s.view().stale);
        assert_eq!(s.view().draft, "x");
        assert!(s.row(&MessageId::new(9)).is_some());
        assert_eq!(
            mutations(&present(&mut s, vec![MessageId::new(9)], true)),
            0
        );
    }
}

// Break: a stopped Watch retries before its supplied-time floor, or emits multiple live intents.
#[test]
fn stopped_watch_retries_only_after_ten_supplied_seconds_and_stays_single_flight() {
    let mut s = ready(vec![]);
    let _ = watch_connection(&mut s, false, true, 50);
    for elapsed in [
        Duration::from_secs(49),
        Duration::from_secs(50),
        Duration::from_millis(59_999),
    ] {
        let effects = s.reduce(
            ChatInput::Tick,
            ChatTime {
                utc: time(5000).utc,
                elapsed,
            },
        );
        assert!(!effects
            .iter()
            .any(|e| matches!(e, ChatEffect::Watch { .. })));
    }
    let first = s.reduce(ChatInput::Tick, time(60));
    assert_eq!(
        first
            .iter()
            .filter(|e| matches!(e, ChatEffect::Watch { cursor: 8, .. }))
            .count(),
        1,
        "{first:?}"
    );
    assert_eq!(mutations(&first), 0);
    for seconds in [60, 61, 70, 100] {
        assert!(!s
            .reduce(ChatInput::Tick, time(seconds))
            .iter()
            .any(|e| matches!(e, ChatEffect::Watch { .. })));
    }
    let _ = watch_connection(&mut s, false, true, 101);
    assert!(!s
        .reduce(
            ChatInput::Tick,
            ChatTime {
                utc: time(5000).utc,
                elapsed: Duration::from_millis(110_999)
            }
        )
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
    let second = s.reduce(ChatInput::Tick, time(111));
    assert_eq!(
        second
            .iter()
            .filter(|e| matches!(e, ChatEffect::Watch { cursor: 8, .. }))
            .count(),
        1
    );
    assert!(!connection(&mut s, true)
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
    let _ = watch_connection(&mut s, false, true, 120);
    let _ = watch_connection(&mut s, false, false, 121);
    assert!(
        !s.reduce(ChatInput::Tick, time(500))
            .iter()
            .any(|e| matches!(e, ChatEffect::Watch { .. })),
        "too-old input cancels timed retries"
    );
}

// Break: an explicit compatible reconnection cannot recover until the retry timer, or duplicates Watch.
#[test]
fn stopped_watch_can_recover_on_connection_before_retry_deadline() {
    let mut s = ready(vec![]);
    let _ = watch_connection(&mut s, false, true, 50);
    let recovered = watch_connection(&mut s, true, true, 51);
    assert_eq!(
        recovered
            .iter()
            .filter(|e| matches!(e, ChatEffect::Watch { cursor: 8, .. }))
            .count(),
        1
    );
    assert!(recovered
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadThreads { .. })));
    assert!(recovered
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadPage { .. })));
    assert!(!s.status.contains("stopped"));
    assert!(!watch_connection(&mut s, true, true, 52)
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
    assert!(!s
        .reduce(ChatInput::Tick, time(61))
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
}

// Break: a successful retry keeps claiming live updates stopped after a real new event.
#[test]
fn a_live_event_confirms_watch_recovery_only_after_the_retry_was_started() {
    let mut s = ready(vec![]);
    let _ = watch_connection(&mut s, false, true, 50);
    let _ = s.reduce(
        ChatInput::WatchEvent {
            seq: 9,
            kind: "agent.updated".into(),
        },
        time(59),
    );
    assert_eq!(s.status, "live updates stopped; retrying");
    let retry = s.reduce(ChatInput::Tick, time(60));
    assert_eq!(
        retry
            .iter()
            .filter(|e| matches!(e, ChatEffect::Watch { cursor: 9, .. }))
            .count(),
        1
    );
    let live = s.reduce(
        ChatInput::WatchEvent {
            seq: 10,
            kind: "msg.sent".into(),
        },
        time(61),
    );
    assert!(!s.status.contains("stopped"), "{}", s.status);
    assert!(!live.iter().any(|e| matches!(e, ChatEffect::Watch { .. })));
    assert!(!s
        .reduce(ChatInput::Tick, time(100))
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
}

// Break: the public read-only generation disagrees with the frame used to gate redraws.
#[test]
fn generation_accessor_matches_frame_report_and_never_mutates() {
    let mut s = ready(vec![]);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    for input in [
        ChatInput::Tick,
        ChatInput::Resize {
            width: 80,
            height: 24,
        },
        ChatInput::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
    ] {
        let _ = s.reduce(input, time(0));
        let generation = s.generation();
        let mut report = None;
        terminal
            .draw(|frame| report = Some(crate::render(frame, &s)))
            .unwrap();
        assert_eq!(report.unwrap().generation, generation);
        assert_eq!(s.generation, generation);
    }
}

// Break: a failed watch is permanent, or ordinary reconnect creates another live Watch.
#[test]
fn failed_watch_rearms_once_on_the_next_compatible_connection() {
    let mut s = ready(vec![]);
    assert!(!connection(&mut s, true)
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
    let _ = s.reduce(
        ChatInput::WatchEvent {
            seq: 77,
            kind: "msg.sent".into(),
        },
        time(0),
    );
    let failed = s.reduce(
        ChatInput::Connection {
            online: true,
            methods: vec![],
            watch_available: false,
        },
        time(0),
    );
    assert!(failed.is_empty());
    assert!(!s.compatible);
    let recovered = connection(&mut s, true);
    assert_eq!(
        recovered
            .iter()
            .filter(|e| matches!(e, ChatEffect::Watch { cursor: 77, .. }))
            .count(),
        1,
        "{recovered:?}"
    );
    assert!(!s.status.contains("too old"));
    assert!(!connection(&mut s, true)
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
    let _ = s.reduce(
        ChatInput::Connection {
            online: false,
            methods: vec![],
            watch_available: false,
        },
        time(1),
    );
    assert!(!connection(&mut s, true)
        .iter()
        .any(|e| matches!(e, ChatEffect::Watch { .. })));
}

// Synthetic: independent final-read concrete cases; pure ordered inputs/time.
fn methods_with_watch(marker: bool) -> Vec<String> {
    let mut methods = [
        "org.tree",
        "msg.history",
        "msg.inbox",
        "msg.send",
        "msg.ack",
    ]
    .map(str::to_owned)
    .to_vec();
    if marker {
        methods.push(WATCH_CAPABILITY.into());
    }
    methods
}
fn connection_at(
    s: &mut ChatState,
    online: bool,
    available: bool,
    marker: bool,
    seconds: u64,
) -> Vec<ChatEffect> {
    s.reduce(
        ChatInput::Connection {
            online,
            methods: methods_with_watch(marker),
            watch_available: available,
        },
        time(seconds),
    )
}

#[test]
fn escape_escape_q_retains_an_unknown_send_lock_and_warning() {
    let mut s = ready(vec![]);
    let token = send_token(&enter_body(&mut s, "must not disappear"));
    let _ = s.reduce(ChatInput::SendDispatched(token), time(0));
    let _ = s.reduce(
        ChatInput::SendResult(
            token,
            Err(ChatFailure::OutcomeUnknown("lost response".into())),
        ),
        time(1),
    );
    for code in [KeyCode::Esc, KeyCode::Esc, KeyCode::Char('Q')] {
        let effects = key(&mut s, code, KeyModifiers::NONE, 1);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, ChatEffect::Quit { .. } | ChatEffect::Unwatch)));
        assert!(!s.closed);
        assert_eq!(s.view().draft, "must not disappear");
        assert!(s.draft_locked());
        assert!(s.status.contains("send may still arrive"));
    }
}

#[test]
fn refused_precheck_with_working_message_reads_does_not_self_sustain_an_ack_cycle() {
    let mut s = ready(vec![row(9, "alpha/chief", "owner", "unread reply")]);
    let mut ack = present(&mut s, vec![MessageId::new(9)], true);
    let mut count = 0;
    let mut generations = vec![];
    // Model the final read's exact failure: every mutation precheck immediately
    // refuses, while msg.history/msg.inbox still answer with fresh unread facts.
    // There are no ticks, keys, Watch events, changes to canonical rows or time.
    for _ in 0..20 {
        let Some(token) = ack.iter().find_map(|e| match e {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        }) else {
            break;
        };
        count += 1;
        let effects = s.reduce(
            ChatInput::AckResult(
                token,
                Err(ChatFailure::Refused(
                    "not sent: session registry unavailable".into(),
                )),
            ),
            time(0),
        );
        assert_eq!(
            mutations(&present(&mut s, vec![MessageId::new(9)], true)),
            0,
            "the requested redraw-only suppression does hold"
        );
        for effect in effects {
            match effect {
                ChatEffect::LoadThreads { token } => {
                    let _ = s.reduce(
                        ChatInput::ThreadsLoaded(
                            token,
                            Err(ChatFailure::Unavailable(
                                "session registry unavailable".into(),
                            )),
                        ),
                        time(0),
                    );
                }
                ChatEffect::LoadPage { token, .. } => {
                    let _ = s.reduce(
                        ChatInput::PageLoaded(
                            token,
                            Ok(page(
                                vec![row(9, "alpha/chief", "owner", "unread reply")],
                                20,
                                None,
                            )),
                        ),
                        time(0),
                    );
                }
                ChatEffect::LoadInbox { token, .. } => {
                    let _ = s.reduce(
                        ChatInput::InboxLoaded(
                            token,
                            Ok(InboxResult {
                                messages: vec![row(9, "alpha/chief", "owner", "unread reply")],
                                unread_count: 1,
                                next_after: None,
                            }),
                        ),
                        time(0),
                    );
                }
                _ => {}
            }
        }
        ack = present(&mut s, vec![MessageId::new(9)], true);
        generations.push(s.generation());
    }
    eprintln!("presentation generations after each automatic retry: {generations:?}");
    eprintln!("{count} refused Ack attempts from the automatic AckResult -> message refresh -> Presented cycle, without any tick, key, watch event, canonical change or elapsed-time advance");
    assert!(count <= 1, "fresh message reads alone must not recreate the final-read's self-sustaining failed-precheck RPC cycle");
    assert_eq!(count, 1);
    let refresh = s.reduce(ChatInput::Tick, time(10));
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)),
        0,
        "time alone cannot replace fresh attribution"
    );
    let loaded = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&refresh), Ok(facts(false))),
        time(10),
    );
    assert_eq!(mutations(&loaded), 0);
    count += mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10));
    assert_eq!(
        count, 2,
        "exactly one more attempt after ten supplied seconds AND a fresh unbound tree"
    );
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)),
        0
    );
}

#[test]
fn known_unsupported_watch_stays_too_old_through_initial_tree_success() {
    let mut s = ChatState::new(ChatConfig {
        initial_thread: Some(address("alpha/chief")),
        caller_pane: None,
    });
    let _ = connection_at(&mut s, true, false, false, 0);
    assert!(s.status.contains("too old"));
    let start = s.reduce(ChatInput::Start, time(0));
    let effects = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&start), Ok(facts(false))),
        time(0),
    );
    eprintln!(
        "known unsupported startup: status={:?}, compatible={}, watch_effects={}",
        s.status,
        s.compatible,
        effects
            .iter()
            .filter(|e| matches!(e, ChatEffect::Watch { .. }))
            .count()
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, ChatEffect::Watch { .. })),
        "known too-old support is not a live-watch recovery"
    );
    assert!(
        s.status.contains("too old"),
        "a successful org.tree must not erase the known missing-watch warning"
    );
}

#[test]
fn ordinary_connection_flapping_does_not_duplicate_a_retry_intent() {
    let mut s = ready(vec![]);
    let _ = connection_at(&mut s, true, false, true, 50);
    let before = s.reduce(
        ChatInput::Tick,
        ChatTime {
            utc: time(999).utc,
            elapsed: Duration::from_millis(59_999),
        },
    );
    assert!(!before.iter().any(|e| matches!(e, ChatEffect::Watch { .. })));
    let retry = s.reduce(ChatInput::Tick, time(60));
    assert_eq!(
        retry
            .iter()
            .filter(|e| matches!(e, ChatEffect::Watch { .. }))
            .count(),
        1
    );
    for _ in 0..20 {
        // The watch is live/intended; transport reconnects are ControlState's job.
        for online in [true, false, true] {
            let effects = connection_at(&mut s, online, true, true, 60);
            assert!(!effects
                .iter()
                .any(|e| matches!(e, ChatEffect::Watch { .. })));
        }
        assert!(!s
            .reduce(ChatInput::Tick, time(60))
            .iter()
            .any(|e| matches!(e, ChatEffect::Watch { .. })));
    }
    assert!(!s.status.contains("stopped"));
}

fn present_at(
    state: &mut ChatState,
    ids: Vec<MessageId>,
    visible: bool,
    seconds: u64,
) -> Vec<ChatEffect> {
    state.reduce(
        ChatInput::Presented {
            generation: state.generation,
            visible_ids: ids,
            visible,
            hint: (!visible).then(|| "auto-ack paused: unfocused".into()),
        },
        time(seconds),
    )
}
fn ack_token(effects: &[ChatEffect]) -> OpToken {
    effects
        .iter()
        .find_map(|effect| match effect {
            ChatEffect::Ack { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap()
}
fn finish_refusal_messages(
    state: &mut ChatState,
    effects: &[ChatEffect],
    rows: &[InboxEntry],
    seconds: u64,
) {
    for effect in effects {
        let input = match effect {
            ChatEffect::LoadPage { token, .. } => {
                ChatInput::PageLoaded(*token, Ok(page(rows.to_vec(), 21, None)))
            }
            ChatEffect::LoadInbox { token, .. } => ChatInput::InboxLoaded(
                *token,
                Ok(InboxResult {
                    messages: rows.to_vec(),
                    unread_count: rows.len(),
                    next_after: None,
                }),
            ),
            _ => continue,
        };
        let completed = state.reduce(input, time(seconds));
        assert!(
            completed.is_empty(),
            "a message result cannot start a mutation/recovery loop: {completed:?}"
        );
    }
}

// Break: fresh message/tree facts or newly arriving IDs bypass the global refusal time floor.
#[test]
fn refused_ack_needs_fresh_tree_and_ten_seconds_even_when_message_reads_work() {
    for expire_on_tick in [false, true] {
        let rows = vec![
            row(9, "alpha/chief", "owner", "reply"),
            row(10, "alpha/chief", "owner", "new unread"),
        ];
        let mut s = ready(vec![rows.first().unwrap().clone()]);
        let _ = plain(&mut s, 'i');
        let _ = plain(&mut s, 'x');
        let first = ack_token(&present_at(&mut s, vec![MessageId::new(9)], true, 0));
        let reads = s.reduce(
            ChatInput::AckResult(
                first,
                Err(ChatFailure::Refused("registry unavailable".into())),
            ),
            time(0),
        );
        assert!(reads
            .iter()
            .any(|e| matches!(e, ChatEffect::LoadThreads { .. })));
        assert!(!s.tree_fresh);
        assert!(s.status.contains("checking who you are"));
        assert!(key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 0).is_empty());
        assert_eq!(s.view().draft, "x");
        finish_refusal_messages(&mut s, &reads, &rows, 0);
        assert_eq!(
            mutations(&present_at(
                &mut s,
                vec![MessageId::new(9), MessageId::new(10)],
                true,
                0
            )),
            0
        );
        let loaded = s.reduce(
            ChatInput::ThreadsLoaded(tree_token(&reads), Ok(facts(false))),
            time(1),
        );
        assert_eq!(mutations(&loaded), 0);
        assert!(
            s.can_compose(),
            "Send needs fresh attribution, not the Ack cooldown"
        );
        for _ in 0..20 {
            assert_eq!(
                mutations(&present_at(
                    &mut s,
                    vec![MessageId::new(9), MessageId::new(10)],
                    true,
                    1
                )),
                0
            );
        }
        let generation = s.generation();
        let before = s.reduce(
            ChatInput::Tick,
            ChatTime {
                utc: time(5000).utc,
                elapsed: Duration::from_millis(9_999),
            },
        );
        assert_eq!(mutations(&before), 0);
        assert_eq!(s.generation(), generation);
        if expire_on_tick {
            assert_eq!(mutations(&s.reduce(ChatInput::Tick, time(10))), 0);
            assert!(
                s.generation() > generation,
                "eligibility expiry must advance generation for the controller draw gate"
            );
            let ignored = s.reduce(
                ChatInput::Presented {
                    generation,
                    visible_ids: vec![MessageId::new(9), MessageId::new(10)],
                    visible: true,
                    hint: None,
                },
                time(10),
            );
            assert!(ignored.is_empty());
        }
        let second = present_at(
            &mut s,
            vec![MessageId::new(9), MessageId::new(10)],
            true,
            10,
        );
        assert_eq!(mutations(&second), 1);
        assert!(
            matches!(second.as_slice(), [ChatEffect::Ack { params, .. }] if params.ids == vec![MessageId::new(9), MessageId::new(10)])
        );
        let again = s.reduce(
            ChatInput::AckResult(
                ack_token(&second),
                Err(ChatFailure::Refused("still unavailable".into())),
            ),
            time(10),
        );
        finish_refusal_messages(&mut s, &again, &rows, 10);
        let _ = s.reduce(
            ChatInput::ThreadsLoaded(tree_token(&again), Ok(facts(false))),
            time(10),
        );
        for _ in 0..20 {
            assert_eq!(
                mutations(&present_at(
                    &mut s,
                    vec![MessageId::new(9), MessageId::new(10)],
                    true,
                    10
                )),
                0
            );
        }
        assert_eq!(
            mutations(&present_at(
                &mut s,
                vec![MessageId::new(9), MessageId::new(10)],
                true,
                19
            )),
            0
        );
        assert_eq!(
            mutations(&present_at(
                &mut s,
                vec![MessageId::new(9), MessageId::new(10)],
                true,
                20
            )),
            1
        );
    }
}

// Break: an attribution read started before refusal or a fresh bound reply authorizes mutations.
#[test]
fn a_pre_refusal_tree_and_bound_facts_cannot_restore_mutation_authority() {
    let rows = vec![row(9, "alpha/chief", "owner", "reply")];
    let mut s = ready(rows.clone());
    let _ = plain(&mut s, 'i');
    let _ = plain(&mut s, 'x');
    let first = ack_token(&present_at(&mut s, vec![MessageId::new(9)], true, 0));
    let old = s.reduce(
        ChatInput::WatchEvent {
            seq: 21,
            kind: "agent.updated".into(),
        },
        time(0),
    );
    let reads = s.reduce(
        ChatInput::AckResult(
            first,
            Err(ChatFailure::Refused("registry unavailable".into())),
        ),
        time(0),
    );
    finish_refusal_messages(&mut s, &reads, &rows, 0);
    let _ = s.reduce(ChatInput::Tick, time(10));
    let fresh = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&old), Ok(facts(false))),
        time(10),
    );
    assert!(!s.tree_fresh);
    assert!(key(&mut s, KeyCode::Enter, KeyModifiers::NONE, 10).is_empty());
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)),
        0
    );
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&fresh), Ok(facts(true))),
        time(10),
    );
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)),
        0
    );
    assert!(!s.can_compose());
    let unbound = s.reduce(
        ChatInput::WatchEvent {
            seq: 22,
            kind: "agent.updated".into(),
        },
        time(10),
    );
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&unbound), Ok(facts(false))),
        time(10),
    );
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 10)),
        1
    );
}

// Break: failed tree recovery is either permanent or becomes a new redraw-driven RPC loop.
#[test]
fn refused_attribution_retries_are_bounded_by_supplied_time() {
    let rows = vec![row(9, "alpha/chief", "owner", "reply")];
    let mut s = ready(rows.clone());
    let first = ack_token(&present_at(&mut s, vec![MessageId::new(9)], true, 0));
    let reads = s.reduce(
        ChatInput::AckResult(
            first,
            Err(ChatFailure::Refused("registry unavailable".into())),
        ),
        time(0),
    );
    assert!(reads
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadThreads { .. })));
    finish_refusal_messages(&mut s, &reads, &rows, 0);
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(
            tree_token(&reads),
            Err(ChatFailure::Unavailable("registry unavailable".into())),
        ),
        time(0),
    );
    assert!(s.status.contains("checking who you are"));
    for _ in 0..20 {
        assert!(present_at(&mut s, vec![MessageId::new(9)], true, 0).is_empty());
        assert!(s.reduce(ChatInput::Tick, time(0)).is_empty());
    }
    assert!(s
        .reduce(
            ChatInput::Tick,
            ChatTime {
                utc: time(999).utc,
                elapsed: Duration::from_millis(9_999)
            }
        )
        .is_empty());
    let retry = s.reduce(ChatInput::Tick, time(10));
    assert!(matches!(retry.as_slice(), [ChatEffect::LoadThreads { .. }]));
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(
            tree_token(&retry),
            Err(ChatFailure::Unavailable("still unavailable".into())),
        ),
        time(10),
    );
    for _ in 0..20 {
        assert!(present_at(&mut s, vec![MessageId::new(9)], true, 10).is_empty());
        assert!(s.reduce(ChatInput::Tick, time(10)).is_empty());
    }
    assert!(s.reduce(ChatInput::Tick, time(19)).is_empty());
    let recovered = s.reduce(ChatInput::Tick, time(20));
    assert!(matches!(
        recovered.as_slice(),
        [ChatEffect::LoadThreads { .. }]
    ));
    let _ = s.reduce(
        ChatInput::ThreadsLoaded(tree_token(&recovered), Ok(facts(false))),
        time(20),
    );
    assert_eq!(
        mutations(&present_at(&mut s, vec![MessageId::new(9)], true, 20)),
        1
    );
}

// Break: initial tree success erases either known unsupported or stopped Watch capability status.
#[test]
fn known_stopped_and_unsupported_startup_keep_their_capability_status() {
    for supported in [false, true] {
        let mut s = ChatState::new(ChatConfig {
            initial_thread: Some(address("alpha/chief")),
            caller_pane: None,
        });
        let _ = connection_at(&mut s, true, false, supported, 0);
        let expected = if supported {
            "live updates stopped; retrying"
        } else {
            "daemon too old: chat methods/watch unavailable · read only"
        };
        let start = s.reduce(ChatInput::Start, time(0));
        let loaded = s.reduce(
            ChatInput::ThreadsLoaded(tree_token(&start), Ok(facts(false))),
            time(0),
        );
        assert_eq!(s.status, expected);
        assert!(!loaded.iter().any(|e| matches!(e, ChatEffect::Watch { .. })));
        let tick = s.reduce(ChatInput::Tick, time(10));
        assert_eq!(
            tick.iter()
                .filter(|e| matches!(e, ChatEffect::Watch { .. }))
                .count(),
            usize::from(supported)
        );
    }
}
