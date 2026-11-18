//! Synthetic decisions and a private protocol peer; no harness or live socket.

// Reuse the private peer and its event guards without widening test-helper visibility.
#[path = "tui_work_tests.rs"]
mod tui_work_tests;
use super::{tui_decision_popup::*, tui_decisions::*};
use atm_core::SessionView;
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};
use atm_tui::{
    client::{DaemonClient, DaemonConfig},
    daemon::ControlHandle,
    input::{ClientCommand, Event},
};
use atmd::org::decisions::DecisionEntry;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::{json, Value};
use std::{future::Future, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixListener,
    },
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

fn entry(id: u64, requester: &str) -> DecisionEntry {
    serde_json::from_value(json!({
        "id":format!("d{id}"), "requested_at":"2026-01-15T12:00:00Z", "status":"pending",
        "request":{"requester":requester,"kind":"plan","title":"Approve the smaller interface?",
            "context":"Read the whole plan.\nSecond line: é🙂", "files":["plans/shape.md"],
            "options":[" yes ","No"],"recommended":" yes ","may_be_undone":true,
            "changes_intent":true,"deadline":"2027-01-01T00:00:00Z"},
        "provisional":null,"taken_at":null,"resolution":null,"withdrawal":null,"closed_at":null,
        "class":"changes_intent"
    }))
    .unwrap()
}
fn tree() -> Value {
    let mut value: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/atm_org/tree.json")).unwrap();
    value["seq"] = json!(41);
    value["projects"][0]["seats"][0]["pane"] = json!("%7");
    value
}
fn page(entries: Vec<DecisionEntry>, next: Option<u64>) -> Value {
    json!({"decisions":entries,"pending_count":entries.len(),"provisional_count":0,
        "next_after":next.map(|n| format!("d{n}"))})
}
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn popup(entry: DecisionEntry) -> DecisionPopup {
    let mut popup = DecisionPopup::default();
    popup.reduce(Input::Loaded(Box::new(entry)));
    popup
}
fn rendered(popup: &mut DecisionPopup, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| popup.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content
        .chunks(usize::from(width).max(1))
        .map(|row| {
            row.iter()
                .map(|c| c.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Break caught: modal escape leaks through as Quit, or repeats select a new request.
#[test]
fn popup_chooser_navigation_escape_and_repeat_are_modal() {
    let mut popup = DecisionPopup::default();
    popup.reduce(Input::Queue(vec![
        entry(1, "alpha/lead"),
        entry(2, "beta/lead"),
    ]));
    assert!(popup.reduce(Input::Key(key(KeyCode::Char('q')))).is_empty());
    popup.reduce(Input::Key(key(KeyCode::Down)));
    let mut repeated = key(KeyCode::Enter);
    repeated.kind = KeyEventKind::Repeat;
    assert!(popup.reduce(Input::Key(repeated)).is_empty());
    assert!(
        matches!(popup.reduce(Input::Key(key(KeyCode::Enter))).as_slice(), [Effect::Get(id)] if id.to_string()=="d2")
    );
    popup.reduce(Input::Loaded(Box::new(entry(2, "beta/lead"))));
    assert!(popup.reduce(Input::Key(key(KeyCode::Esc))).is_empty());
    assert!(matches!(popup.screen, Screen::Queue));
    assert!(matches!(
        popup.reduce(Input::Key(key(KeyCode::Esc))).as_slice(),
        [Effect::Close]
    ));
    assert!(matches!(
        popup
            .reduce(Input::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            )))
            .as_slice(),
        [Effect::Quit]
    ));
}

// Break caught: controls execute/spoof rows or long context/options cannot be reached.
#[test]
fn popup_full_context_unicode_controls_and_resize_are_readable() {
    let mut item = entry(41, "alpha/lead");
    item.decision.request.title = "Question\u{1b}[2J\t\\".into();
    item.decision.request.context = (0..100).map(|n| format!("context {n} é🙂\n")).collect();
    item.decision.request.options = (0..32).map(|n| format!(" exact option {n} ")).collect();
    let mut popup = popup(item);
    let first = rendered(&mut popup, 52, 16);
    assert!(first.contains("Question\\u{1b}[2J\\t\\\\"), "{first}");
    for _ in 0..1000 {
        popup.reduce(Input::Key(key(KeyCode::Down)));
    }
    let bottom = rendered(&mut popup, 52, 16);
    assert!(bottom.contains("exact option 31"), "{bottom}");
    popup.reduce(Input::Key(key(KeyCode::Up)));
    assert_ne!(rendered(&mut popup, 52, 16), bottom);
    rendered(&mut popup, 1, 1);
    rendered(&mut popup, 0, 0);
    let expanded = rendered(&mut popup, 100, 200);
    assert!(expanded.contains("context 99 é🙂"));
    assert!(!expanded.chars().any(|c| c.is_control() && c != '\n'));
}

#[test]
fn decision_inspect_snapshot() {
    insta::assert_snapshot!(
        "decision_inspect",
        rendered(&mut popup(entry(41, "alpha/lead")), 80, 32)
    );
}
#[test]
fn decision_chooser_snapshot() {
    let mut chooser = DecisionPopup::default();
    chooser.reduce(Input::Queue(vec![
        entry(41, "alpha/lead"),
        entry(42, "beta/lead"),
    ]));
    insta::assert_snapshot!("decision_chooser", rendered(&mut chooser, 72, 14));
}
#[test]
fn decision_narrow_snapshot() {
    insta::assert_snapshot!(
        "decision_narrow",
        rendered(&mut popup(entry(41, "alpha/lead")), 28, 16)
    );
}
#[test]
fn decision_empty_snapshot() {
    let mut empty = DecisionPopup::default();
    empty.reduce(Input::Queue(vec![]));
    insta::assert_snapshot!("decision_empty", rendered(&mut empty, 60, 10));
}
#[test]
fn decision_loading_snapshot() {
    insta::assert_snapshot!(
        "decision_loading",
        rendered(&mut DecisionPopup::default(), 60, 10)
    );
}
#[test]
fn decision_unavailable_snapshot() {
    let mut loading = DecisionPopup::default();
    loading.reduce(Input::Failed(
        "Decision service unavailable: old daemon".into(),
    ));
    insta::assert_snapshot!("decision_unavailable", rendered(&mut loading, 60, 10));
}

async fn guarded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .expect("hung decision proof")
}
struct Peer {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    watches: Vec<(Vec<String>, u64)>,
}
impl Peer {
    async fn raw(&mut self) -> MessageType {
        let mut line = String::new();
        assert_ne!(guarded(self.reader.read_line(&mut line)).await.unwrap(), 0);
        serde_json::from_str::<ClientMessage>(&line)
            .unwrap()
            .message
    }
    async fn watch_at(&mut self, cursor: u64) {
        while !self.watches.iter().any(|(_, seq)| *seq == cursor) {
            match self.raw().await {
                MessageType::Watch { topics, cursor } => self.watches.push((topics, cursor)),
                MessageType::Unwatch => {}
                MessageType::Ping { seq } => self.send(DaemonMessage::Pong { seq }).await,
                other => panic!("expected watch barrier, got {other:?}"),
            }
        }
    }
    async fn recv(&mut self) -> MessageType {
        loop {
            match self.raw().await {
                MessageType::Watch { topics, cursor } => self.watches.push((topics, cursor)),
                MessageType::Unwatch => {}
                MessageType::Ping { seq } => self.send(DaemonMessage::Pong { seq }).await,
                other => return other,
            }
        }
    }
    async fn fence(&mut self, commands: &mpsc::UnboundedSender<ClientCommand>) {
        commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(self.recv().await, MessageType::Discover));
    }
    async fn send(&mut self, message: DaemonMessage) {
        guarded(
            self.writer
                .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes()),
        )
        .await
        .unwrap();
    }
    async fn request(&mut self, method: &str) -> (String, Value) {
        let MessageType::Request {
            id,
            method: got,
            params,
            from_pane,
        } = self.recv().await
        else {
            panic!("expected request")
        };
        assert_eq!(got, method);
        assert_eq!(
            from_pane.as_deref(),
            Some("%99"),
            "must send TUI caller, never selected pane"
        );
        (id, params)
    }
    async fn reply(&mut self, method: &str, result: Value) -> Value {
        let (id, params) = self.request(method).await;
        self.send(DaemonMessage::response_ok(id, result)).await;
        params
    }
}
struct Rig {
    _dir: tempfile::TempDir,
    cancel: CancellationToken,
    run: tokio::task::JoinHandle<()>,
    commands: mpsc::UnboundedSender<ClientCommand>,
    _events: mpsc::UnboundedReceiver<Event>,
}
impl Rig {
    async fn start(methods: &[&str]) -> (Self, Peer) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (commands, rx) = mpsc::unbounded_channel();
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let client = DaemonClient::new(
            DaemonConfig {
                socket_path: path,
                ..Default::default()
            },
            event_tx,
            rx,
            cancel.clone(),
        );
        let run = tokio::spawn(async move { client.run().await });
        let (stream, _) = guarded(listener.accept()).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut peer = Peer {
            reader: BufReader::new(reader),
            writer,
            watches: Vec::new(),
        };
        assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
        peer.send(DaemonMessage::Connected {
            protocol_version: ProtocolVersion::new(1, 1),
            client_id: "decision-peer".into(),
            methods: methods.iter().map(|m| (*m).into()).collect(),
        })
        .await;
        assert!(matches!(peer.recv().await, MessageType::Subscribe { .. }));
        peer.send(DaemonMessage::session_list(vec![])).await;
        loop {
            if matches!(
                guarded(events.recv()).await.unwrap(),
                Event::SessionListReplace(_)
            ) {
                break;
            }
        }
        (
            Self {
                _dir: dir,
                cancel,
                run,
                commands,
                _events: events,
            },
            peer,
        )
    }
    fn controller(&self) -> DecisionController {
        DecisionController::new(
            ControlHandle::new(self.commands.clone()),
            Some("%99".into()),
        )
    }
    async fn stop(mut self) {
        self.cancel.cancel();
        guarded(&mut self.run).await.unwrap();
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.run.abort();
    }
}
async fn apply_next(controller: &mut DecisionController) {
    let update = guarded(controller.recv()).await.unwrap();
    controller.apply(update);
}
async fn apply_popup(controller: &mut DecisionController) {
    loop {
        let update = guarded(controller.recv()).await.unwrap();
        let popup = matches!(update, Update::Popup { .. });
        controller.apply(update);
        if popup {
            break;
        }
    }
}
fn selected(pane: &str) -> SessionView {
    SessionView {
        id: "selected".into(),
        tmux_pane: Some(pane.into()),
        ..Default::default()
    }
}
// Break caught: Enter cannot submit an exact option from an inspect popup.
#[test]
fn reversible_answer_is_an_exact_single_effect() {
    let mut popup = popup(entry(41, "alpha/lead"));
    popup.reduce(Input::Key(key(KeyCode::Char('k'))));
    let effects = popup.reduce(Input::Key(key(KeyCode::Enter)));
    assert_eq!(effects.len(), 1);
    let effect = format!("{:?}", effects.first());
    assert!(
        effect.contains("Resolve") && effect.contains(" yes "),
        "{effect}"
    );
    assert!(
        popup.reduce(Input::Key(key(KeyCode::Enter))).is_empty(),
        "duplicate submission"
    );
}

// Break caught: reversible flags bypass confirmation for outside-facing kinds,
// or Enter/Repeat confirms a default-No prompt.
#[test]
fn irreversible_kinds_and_provisional_overrule_require_explicit_y() {
    use atm_org::{DecisionKind, DecisionStatus, ProvisionalChoice};
    for kind in [
        DecisionKind::Action,
        DecisionKind::Hire,
        DecisionKind::Budget,
        DecisionKind::Unknown,
    ] {
        let mut item = entry(41, "alpha/lead");
        item.decision.request.kind = kind;
        item.decision.request.may_be_undone = true;
        let mut view = popup(item);
        view.reduce(Input::Key(key(KeyCode::Char('k'))));
        assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
        let text = rendered(&mut view, 100, 32);
        assert!(
            text.contains("Confirm d41") && text.contains("[y/N]"),
            "{text}"
        );
        assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
        view.reduce(Input::Key(key(KeyCode::Enter)));
        let mut repeat = key(KeyCode::Char('y'));
        repeat.kind = KeyEventKind::Repeat;
        assert!(view.reduce(Input::Key(repeat)).is_empty());
        assert_eq!(view.reduce(Input::Key(key(KeyCode::Char('y')))).len(), 1);
    }
    let mut item = entry(41, "alpha/lead");
    item.decision.status = DecisionStatus::TakenProvisionally;
    item.decision.provisional = Some(ProvisionalChoice {
        relayed_by: None,
        decision_id: item.decision.id,
        option: " yes ".into(),
        by: atm_org::Address::System,
    });
    let mut view = popup(item);
    view.reduce(Input::Key(key(KeyCode::Char('k'))));
    view.reduce(Input::Key(key(KeyCode::Char('j'))));
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    let text = rendered(&mut view, 100, 32);
    assert!(
        text.contains("reopens and unassigns") && text.contains("No files are reverted"),
        "{text}"
    );
    assert_eq!(view.reduce(Input::Key(key(KeyCode::Char('y')))).len(), 1);
}

#[test]
fn note_is_single_line_utf8_bounded_and_options_are_not_rewritten() {
    let mut view = popup(entry(41, "alpha/lead"));
    view.reduce(Input::Key(key(KeyCode::Char('k'))));
    view.reduce(Input::Key(key(KeyCode::Tab)));
    for _ in 0..2048 {
        view.reduce(Input::Key(key(KeyCode::Char('🙂'))));
    }
    view.reduce(Input::Key(key(KeyCode::Char('x')))); // beyond 8192 bytes
    view.reduce(Input::Key(key(KeyCode::Backspace)));
    view.reduce(Input::Key(key(KeyCode::Char('é'))));
    view.reduce(Input::Key(key(KeyCode::Char('\t')))); // never insert a control
    assert!(
        view.reduce(Input::Key(key(KeyCode::Enter))).is_empty(),
        "Enter is ignored while editing, not approval"
    );
    view.reduce(Input::Key(key(KeyCode::Esc)));
    let effects = view.reduce(Input::Key(key(KeyCode::Enter)));
    let [Effect::Resolve(params)] = effects.as_slice() else {
        panic!("one answer")
    };
    assert_eq!(params.option, " yes ");
    assert_eq!(
        params.note.as_ref().unwrap(),
        &format!("{}é", "🙂".repeat(2047))
    );
    assert_eq!(params.note.as_ref().unwrap().len(), 8190);
    assert!(!params.standing_rule);
}

#[test]
fn already_final_withdrawn_and_expired_decisions_cannot_be_submitted() {
    use atm_org::DecisionStatus;
    for status in [
        DecisionStatus::Resolved,
        DecisionStatus::Overruled,
        DecisionStatus::Withdrawn,
        DecisionStatus::Expired,
    ] {
        let mut item = entry(41, "alpha/lead");
        item.decision.status = status;
        let mut view = popup(item);
        view.reduce(Input::Key(key(KeyCode::Char('k'))));
        assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
        assert!(view.reduce(Input::Key(key(KeyCode::Char('y')))).is_empty());
        assert!(rendered(&mut view, 100, 32).contains("not waiting"));
    }
}

fn resolved(
    mut item: DecisionEntry,
    option: &str,
    overruled: bool,
) -> atmd::org::decisions::MutationResult {
    use atm_org::{Address, DecisionResolution, DecisionStatus};
    item.decision.status = if overruled {
        DecisionStatus::Overruled
    } else {
        DecisionStatus::Resolved
    };
    item.decision.resolution = Some(DecisionResolution {
        decision_id: item.decision.id,
        option: option.into(),
        by: Address::Owner,
        note: None,
        standing_rule: false,
    });
    atmd::org::decisions::MutationResult {
        decision: item,
        changed: true,
        redo: None,
    }
}
#[test]
fn decision_confirmation_snapshot() {
    let mut item = entry(41, "alpha/lead");
    item.decision.request.kind = atm_org::DecisionKind::Action;
    let mut view = popup(item);
    view.reduce(Input::Key(key(KeyCode::Char('k'))));
    view.reduce(Input::Key(key(KeyCode::Enter)));
    insta::assert_snapshot!("decision_confirmation", rendered(&mut view, 80, 30));
}
#[test]
fn decision_recorded_snapshot() {
    let mut view = popup(entry(41, "alpha/lead"));
    view.reduce(Input::Recorded(
        Box::new(resolved(entry(41, "alpha/lead"), " yes ", false)),
        None,
    ));
    insta::assert_snapshot!("decision_recorded", rendered(&mut view, 80, 14));
}
#[test]
fn decision_overrule_snapshot() {
    let mut result = resolved(entry(41, "alpha/lead"), "No", true);
    result.changed = false;
    result.redo=Some(serde_json::from_value(json!({"required_count":2,"completed_count":1,"cancelled_count":0,"task_ids":["t8","t9"],"next_after":null})).unwrap());
    let mut view = popup(entry(41, "alpha/lead"));
    view.reduce(Input::Recorded(Box::new(result), None));
    insta::assert_snapshot!("decision_overrule", rendered(&mut view, 80, 16));
}
#[test]
fn decision_outcome_unknown_snapshot() {
    let mut view = popup(entry(41, "alpha/lead"));
    view.reduce(Input::OutcomeUnknown(
        "daemon disconnected; request was not retried".into(),
    ));
    insta::assert_snapshot!("decision_outcome_unknown", rendered(&mut view, 80, 32));
}

async fn open_peer_entry(
    controller: &mut DecisionController,
    peer: &mut Peer,
    item: DecisionEntry,
) {
    controller.open(item.decision.id);
    peer.reply("decision.get", json!({"decision":item})).await;
    apply_next(controller).await;
    controller.key(key(KeyCode::Char('k')));
}

#[tokio::test]
async fn resolve_is_single_in_flight_and_refusal_retains_draft_for_inspection() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    controller.key(key(KeyCode::Tab));
    for c in "owner note é".chars() {
        controller.key(key(KeyCode::Char(c)));
    }
    controller.key(key(KeyCode::Esc));
    controller.key(key(KeyCode::Enter));
    let (id, params) = peer.request("decision.resolve").await;
    assert_eq!(
        params,
        json!({"id":"d41","option":" yes ","note":"owner note é","standing_rule":false})
    );
    controller.key(key(KeyCode::Enter));
    controller.key(key(KeyCode::Char('y')));
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    peer.send(DaemonMessage::response_err(
        id,
        "refused",
        "only the owner may resolve a decision",
    ))
    .await;
    apply_next(&mut controller).await;
    let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
    assert!(
        text.contains("unbound owner shell") && text.contains("owner note é"),
        "{text}"
    );
    controller.key(key(KeyCode::Enter));
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    controller.key(key(KeyCode::Char('i')));
    peer.reply("decision.get", json!({"decision":entry(41,"alpha/lead")}))
        .await;
    apply_next(&mut controller).await;
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 32).contains("owner note é"));
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn accepted_resolve_with_lost_or_malformed_reply_never_retries() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    controller.key(key(KeyCode::Enter));
    let (id, _) = peer.request("decision.resolve").await;
    // A syntactically valid but wrong decision is not a successful answer.
    peer.send(DaemonMessage::response_ok(
        id,
        serde_json::to_value(resolved(entry(42, "alpha/lead"), " yes ", false)).unwrap(),
    ))
    .await;
    apply_next(&mut controller).await;
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
    controller.key(key(KeyCode::Enter));
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    controller.key(key(KeyCode::Char('i')));
    peer.reply("decision.get", json!({"decision":entry(41,"alpha/lead")}))
        .await;
    apply_next(&mut controller).await;
    controller.key(key(KeyCode::Enter));
    peer.request("decision.resolve").await;
    drop(peer);
    apply_next(&mut controller).await;
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
    controller.key(key(KeyCode::Enter));
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn resolve_timeout_is_unknown_and_shutdown_does_not_replay() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut controller = DecisionController::new(ControlHandle::new(tx), None);
    controller.popup = Some(popup(entry(41, "alpha/lead")));
    controller.key(key(KeyCode::Char('k')));
    controller.key(key(KeyCode::Enter));
    let _held = rx.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(27)).await;
    apply_next(&mut controller).await;
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
    controller.key(key(KeyCode::Enter));
    assert!(rx.try_recv().is_err());
    controller.shutdown();
}

#[tokio::test]
async fn recorded_overrule_drains_redo_and_labels_a_failed_continuation() {
    for fail in [false, true] {
        let (rig, mut peer) = Rig::start(METHODS).await;
        let mut controller = rig.controller();
        let mut item = entry(41, "alpha/lead");
        item.decision.status = atm_org::DecisionStatus::TakenProvisionally;
        item.decision.provisional = Some(atm_org::ProvisionalChoice {
            relayed_by: None,
            decision_id: item.decision.id,
            option: "No".into(),
            by: atm_org::Address::System,
        });
        open_peer_entry(&mut controller, &mut peer, item.clone()).await;
        controller.key(key(KeyCode::Enter));
        controller.key(key(KeyCode::Char('y')));
        let mut result = resolved(item, " yes ", true);
        result.redo=Some(serde_json::from_value(json!({"required_count":2,"completed_count":0,"cancelled_count":0,"task_ids":["t8"],"next_after":"t8"})).unwrap());
        peer.reply("decision.resolve", serde_json::to_value(&result).unwrap())
            .await;
        let (id, params) = peer.request("decision.get").await;
        assert_eq!(params["redo_after"], "t8");
        if fail {
            peer.send(DaemonMessage::response_err(
                id,
                "unavailable",
                "redo unavailable",
            ))
            .await;
        } else {
            peer.send(DaemonMessage::response_ok(id,json!({"decision":result.decision,"redo":{"required_count":2,"completed_count":1,"cancelled_count":0,"task_ids":["t9"],"next_after":null}}))).await;
        }
        apply_next(&mut controller).await;
        let text = rendered(controller.popup.as_mut().unwrap(), 100, 30);
        assert!(
            text.contains("recorded") && text.contains("No files are reverted"),
            "{text}"
        );
        if fail {
            assert!(
                text.contains("Redo report unavailable") && !text.contains("t8"),
                "{text}"
            );
        } else {
            assert!(
                text.contains("t8") && text.contains("t9") && text.contains("1/2"),
                "{text}"
            );
        }
        controller.shutdown();
        rig.stop().await;
    }
}

mod real_daemon {
    use super::*;
    use atm_org::{Address, DecisionStatus, OrgCommand};
    use atm_tui::control::ControlNotice;
    use atmd::{
        org::{agents::AgentHandle, messages::MessageHandle, OrgHandle},
        registry::spawn_registry,
        server::{methods::MethodServices, DaemonServer},
    };
    struct ServerRig {
        _dir: tempfile::TempDir,
        org: OrgHandle,
        cancel: CancellationToken,
        server: tokio::task::JoinHandle<Result<(), atmd::server::ServerError>>,
        client: tokio::task::JoinHandle<()>,
        control: ControlHandle,
        _events: mpsc::UnboundedReceiver<Event>,
    }
    impl ServerRig {
        async fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
            tokio::fs::create_dir_all(dir.path().join(".atm/roles"))
                .await
                .unwrap();
            for (seat, manager) in [("worker", "lead"), ("lead", "owner")] {
                tokio::fs::write(
                    dir.path().join(format!(".atm/roles/{seat}.md")),
                    format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nFixture instructions\n"),
                )
                .await
                .unwrap();
            }
            for command in [
                OrgCommand::RegisterProject {
                    project_id: "project".parse().unwrap(),
                    root: dir.path().to_str().unwrap().into(),
                    move_root: false,
                },
                OrgCommand::Adopt {
                    agent_id: "project/worker".parse().unwrap(),
                    pane: "%7".into(),
                    session_id: None,
                },
                OrgCommand::Adopt {
                    agent_id: "project/lead".parse().unwrap(),
                    pane: "%8".into(),
                    session_id: None,
                },
            ] {
                org.execute(command, chrono::Utc::now()).await.unwrap();
            }
            let mut services = MethodServices::new(spawn_registry());
            services.org = org.clone();
            services.agents = Some(AgentHandle::disabled("fixture"));
            services.messages = Some(MessageHandle::disabled("fixture"));
            let (control, _) = atmd::control::service::spawn_control(
                Box::new(atm_tmux::MockTmuxClient::new()),
                services.registry.clone(),
                org.clone(),
                cancel.clone(),
            );
            services.control = Some(control);
            let path = dir.path().join("atm.sock");
            let server = DaemonServer::with_services(path.clone(), services, cancel.clone());
            let mut run = Box::pin(async move { server.run().await });
            std::future::poll_fn(|cx| {
                use std::task::Poll;
                assert!(run.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let server = tokio::spawn(run);
            let (tx, rx) = mpsc::unbounded_channel();
            let (events_tx, mut events) = mpsc::unbounded_channel();
            let daemon = DaemonClient::new(
                DaemonConfig {
                    socket_path: path,
                    ..Default::default()
                },
                events_tx,
                rx,
                cancel.clone(),
            );
            let client = tokio::spawn(async move { daemon.run().await });
            loop {
                if matches!(
                    guarded(events.recv()).await.unwrap(),
                    Event::SessionListReplace(_)
                ) {
                    break;
                }
            }
            Self {
                _dir: dir,
                org,
                cancel,
                server,
                client,
                control: ControlHandle::new(tx),
                _events: events,
            }
        }
        fn controller(&self, pane: Option<&str>) -> DecisionController {
            DecisionController::new(self.control.clone(), pane.map(str::to_owned))
        }
        async fn call(&self, method: &str, params: Value, pane: Option<&str>) -> Value {
            match guarded(self.control.request(
                method.into(),
                params,
                pane.map(str::to_owned),
                Duration::from_secs(27),
            ))
            .await
            .unwrap()
            {
                atm_protocol::ResponsePayload::Result(value) => value,
                other => panic!("unexpected response: {other:?}"),
            }
        }
        async fn request(&self) -> atm_org::DecisionId {
            let item = entry(1, "project/worker");
            let mut params = serde_json::to_value(item.decision.request).unwrap();
            params.as_object_mut().unwrap().remove("requester");
            let response = self.call("decision.request", params, Some("%7")).await;
            serde_json::from_value(response["decision_id"].clone()).unwrap()
        }
        async fn inspect(&self, controller: &mut DecisionController, id: atm_org::DecisionId) {
            controller.open(id);
            loop {
                apply_popup(controller).await;
                let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
                if matches!(&controller.popup.as_ref().unwrap().screen,
                    Screen::Inspect(entry) if entry.decision.id == id)
                    && !text.contains("refreshing before answering")
                    && !text.contains("Outcome unknown")
                {
                    break;
                }
            }
            controller.key(key(KeyCode::Char('k')));
        }
        async fn stop(self) {
            self.cancel.cancel();
            guarded(self.client).await.unwrap();
            guarded(self.server).await.unwrap().unwrap();
        }
    }
    // Break caught: an owner key does not commit the exact answer/caller audit,
    // or bypasses the canonical durable message bridge by pasting an answer.
    #[tokio::test]
    async fn owner_popup_records_event_and_canonical_answer_without_terminal_delivery() {
        let rig = ServerRig::start().await;
        let id = rig.request().await;
        let cursor = rig.org.status().unwrap();
        let mut resolved = rig
            .org
            .watch(vec!["decision.resolved".into()], cursor)
            .unwrap();
        let mut answers = rig.org.watch(vec!["msg.sent".into()], cursor).unwrap();
        let mut controller = rig.controller(Some("%99"));
        controller.open_for(Some(&selected("%7")));
        apply_next(&mut controller).await;
        apply_next(&mut controller).await;
        controller.key(key(KeyCode::Char('k')));
        controller.key(key(KeyCode::Tab));
        for c in "owner note".chars() {
            controller.key(key(KeyCode::Char(c)));
        }
        controller.key(key(KeyCode::Esc));
        controller.key(key(KeyCode::Enter));
        controller.key(key(KeyCode::Enter));
        apply_next(&mut controller).await;
        let event = guarded(resolved.next()).await.unwrap().unwrap();
        assert_eq!(event.body["decision_id"], id.to_string());
        assert_eq!(event.body["option"], " yes ");
        assert_eq!(event.body["by"], "owner");
        assert_eq!(event.body["caller_pane"], "%99");
        assert_eq!(event.body["note"], "owner note");
        assert_eq!(event.body["standing_rule"], false);
        let answer = guarded(answers.next()).await.unwrap().unwrap();
        assert_eq!(answer.body["from"], "system");
        assert_eq!(answer.body["to"], "project/worker");
        assert_eq!(answer.body["kind"], "answer");
        assert_eq!(
            answer.body["body"],
            format!("Decision {id} resolved:  yes \nNote: owner note\nStanding rule: no")
        );
        let state = rig.org.snapshot().await.unwrap();
        let decision = state.decision(&id).unwrap();
        assert_eq!(decision.status, DecisionStatus::Resolved);
        assert_eq!(decision.resolution.as_ref().unwrap().by, Address::Owner);
        assert_eq!(state.messages().count(), 1);
        let text = rendered(controller.popup.as_mut().unwrap(), 100, 30);
        assert!(
            text.contains("recorded") && !text.contains("delivered"),
            "{text}"
        );
        controller.shutdown();
        rig.stop().await;
    }
    // Break caught: real watched decision events never invalidate the projection,
    // even though a protocol-peer unit test can inject an equivalent notice.
    #[tokio::test]
    async fn real_watch_refreshes_marks_after_request_and_owner_answer() {
        let mut rig = ServerRig::start().await;
        let mut controller = rig.controller(Some("%99"));
        controller.refresh();
        apply_next(&mut controller).await;
        assert!(controller.waiting_panes.is_empty());
        let id = rig.request().await;
        loop {
            if let Event::Control(notice) = guarded(rig._events.recv()).await.unwrap() {
                let requested = matches!(&notice, atm_tui::control::ControlNotice::Event(DaemonMessage::Event {kind,..}) if kind == "decision.requested");
                controller.notice(&notice);
                if requested {
                    break;
                }
            }
        }
        apply_next(&mut controller).await;
        assert_eq!(
            controller.waiting_panes,
            std::collections::HashSet::from(["%7".to_owned()])
        );
        rig.inspect(&mut controller, id).await;
        controller.key(key(KeyCode::Enter));
        apply_popup(&mut controller).await;
        // The confirmed mutation schedules a fresh read even before its watched
        // event is forwarded; await that receipt, never a delay or a deadline poll.
        apply_next(&mut controller).await;
        assert!(controller.waiting_panes.is_empty());
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .decision(&id)
                .unwrap()
                .status,
            DecisionStatus::Resolved
        );
        controller.shutdown();
        rig.stop().await;
    }

    #[tokio::test]
    async fn requester_manager_and_malformed_callers_cannot_record_owner_answers() {
        let rig = ServerRig::start().await;
        let id = rig.request().await;
        for pane in ["%7", "%8"] {
            let mut controller = rig.controller(Some(pane));
            rig.inspect(&mut controller, id).await;
            controller.key(key(KeyCode::Enter));
            apply_next(&mut controller).await;
            assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40)
                .contains("unbound owner shell"));
            controller.shutdown();
        }
        let mut controller = rig.controller(Some("not-a-pane"));
        controller.popup = Some(popup(entry(id.seq(), "project/worker")));
        controller.key(key(KeyCode::Char('k')));
        controller.key(key(KeyCode::Enter));
        apply_next(&mut controller).await;
        assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .decision(&id)
                .unwrap()
                .status,
            DecisionStatus::Pending
        );
        assert_eq!(rig.org.snapshot().await.unwrap().messages().count(), 0);
        controller.shutdown();
        rig.stop().await;
    }
    #[tokio::test]
    async fn stale_popup_exact_repeat_is_unchanged_but_different_final_and_withdrawn_are_refused() {
        let rig = ServerRig::start().await;
        let id = rig.request().await;
        let mut controller = rig.controller(None);
        rig.inspect(&mut controller, id).await;
        rig.call(
            "decision.resolve",
            json!({"id":id,"option":" yes "}),
            Some("%99"),
        )
        .await;
        controller.key(key(KeyCode::Enter));
        apply_popup(&mut controller).await;
        assert!(rendered(controller.popup.as_mut().unwrap(), 100, 24).contains("unchanged"));
        // A fresh get must disable final changes, including a different option.
        rig.inspect(&mut controller, id).await;
        controller.key(key(KeyCode::Char('j')));
        controller.key(key(KeyCode::Enter));
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .decision(&id)
                .unwrap()
                .resolution
                .as_ref()
                .unwrap()
                .option,
            " yes "
        );
        let pending = rig.request().await;
        rig.inspect(&mut controller, pending).await;
        rig.call(
            "decision.resolve",
            json!({"id":pending,"option":"No"}),
            Some("%99"),
        )
        .await;
        controller.key(key(KeyCode::Enter));
        apply_popup(&mut controller).await;
        assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
        let withdrawn = rig.request().await;
        rig.inspect(&mut controller, withdrawn).await;
        rig.call("decision.withdraw", json!({"id":withdrawn}), Some("%99"))
            .await;
        controller.key(key(KeyCode::Enter));
        apply_popup(&mut controller).await;
        assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .decision(&withdrawn)
                .unwrap()
                .status,
            DecisionStatus::Withdrawn
        );
        controller.shutdown();
        rig.stop().await;
    }

    // Adopted from the VW2b review: real private daemon/client/controllers/rendering.
    use crate::tui_work::WorkController;
    use atm_tui::{
        app::{App, AppView},
        work::{WorkEffect, WorkInput},
    };

    fn review_effects(
        app: &mut App,
        work: &mut WorkController,
        owner: &mut DecisionController,
        mut effects: Vec<WorkEffect>,
    ) {
        loop {
            if let Some(seq) = app.work.org().and_then(|org| org.seq) {
                owner.advance_work_cursor(seq);
            }
            match work.execute(effects, owner) {
                Some(input) => effects = app.reduce_work(input, Duration::ZERO),
                None => break,
            }
        }
    }
    fn review_input(
        app: &mut App,
        work: &mut WorkController,
        owner: &mut DecisionController,
        input: WorkInput,
        now: u64,
    ) {
        let effects = app.reduce_work(input, Duration::from_secs(now));
        review_effects(app, work, owner, effects);
    }
    async fn review_step(
        rig: &mut ServerRig,
        app: &mut App,
        work: &mut WorkController,
        owner: &mut DecisionController,
        now: u64,
    ) -> String {
        tokio::select! {
            update = owner.recv() => {
                let update = update.unwrap();
                let kind = if matches!(&update, Update::Snapshot {..}) { "decision.snapshot" } else { "decision.reply" };
                owner.apply(update); kind.into()
            }
            input = work.recv() => {
                let kind = match &input {
                    WorkInput::WindowsReply { .. } => "windows.reply",
                    WorkInput::OrgReply { .. } => "org.reply",
                    _ => "work.wake",
                };
                review_input(app, work, owner, input, now); kind.into()
            }
            event = rig._events.recv() => {
                let Event::Control(notice) = event.unwrap() else { return "session.event".into() };
                let kind = match &notice {
                    ControlNotice::Event(DaemonMessage::Event {kind, ..}) => kind.clone(),
                    _ => "control.notice".into(),
                };
                owner.notice(&notice);
                let input = match notice {
                    ControlNotice::Capabilities {methods, ..} => Some(WorkInput::Capabilities {methods, watch: true}),
                    ControlNotice::Event(DaemonMessage::Event {seq, kind, ..}) => Some(WorkInput::WatchEvent {seq, kind}),
                    ControlNotice::WatchFailed(error) => panic!("unexpected watch failure: {error:?}"),
                    _ => None,
                };
                if let Some(input) = input { review_input(app, work, owner, input, now); }
                kind
            }
        }
    }
    async fn review_show(
        rig: &mut ServerRig,
        app: &mut App,
        work: &mut WorkController,
        owner: &mut DecisionController,
    ) {
        let effects = app.toggle_work_view(Duration::ZERO);
        review_effects(app, work, owner, effects);
        let (mut windows, mut org, mut fresh) = (false, false, false);
        // Work's own replies are insufficient now: await the post-join decision read too.
        while !windows || !org || !fresh {
            match guarded(review_step(rig, app, work, owner, 0))
                .await
                .as_str()
            {
                "windows.reply" => windows = true,
                "org.reply" => org = true,
                "decision.snapshot" => fresh = true,
                _ => {}
            }
        }
        assert_eq!(app.view, AppView::Work);
        assert!(app.work.org().is_some());
    }
    fn review_list(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 12)).unwrap();
        terminal
            .draw(|frame| atm_tui::ui::session_list::render_session_list(frame, frame.area(), app))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(140)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn review_popup_ready(owner: &mut DecisionController, status: DecisionStatus) -> bool {
        matches!(owner.popup.as_ref().map(|p| &p.screen), Some(Screen::Inspect(entry)) if entry.decision.status == status)
            && !rendered(owner.popup.as_mut().unwrap(), 100, 40)
                .contains("refreshing before answering")
    }
    async fn review_body(rig: &mut ServerRig, work_first: bool, hide_first: bool) {
        use atm_org::{
            DecisionCommand, DecisionResolution, ProvisionalChoice, TaskActor, TaskCommand,
            TaskCreated, TaskKind, TaskMeasure,
        };
        let id = rig.request().await;
        let mut owner = rig.controller(Some("%99"));
        owner.refresh();
        apply_next(&mut owner).await;
        let mut app = App::new();
        app.view = AppView::Sessions;
        app.update_sessions(vec![SessionView {
            id: "worker-row".into(),
            id_short: "worker-row".into(),
            tmux_pane: Some("%7".into()),
            ..Default::default()
        }]);
        let mut work = WorkController::new(rig.control.clone(), Some("%99".into()));
        loop {
            if let Event::Control(notice @ ControlNotice::Capabilities { .. }) =
                guarded(rig._events.recv()).await.unwrap()
            {
                owner.notice(&notice);
                let ControlNotice::Capabilities { methods, .. } = notice else {
                    unreachable!()
                };
                review_input(
                    &mut app,
                    &mut work,
                    &mut owner,
                    WorkInput::Capabilities {
                        methods,
                        watch: true,
                    },
                    0,
                );
                break;
            }
        }
        if work_first {
            review_show(rig, &mut app, &mut work, &mut owner).await;
            rig.inspect(&mut owner, id).await;
        } else {
            rig.inspect(&mut owner, id).await;
            review_show(rig, &mut app, &mut work, &mut owner).await;
        }
        while !review_popup_ready(&mut owner, DecisionStatus::Pending) {
            guarded(review_step(rig, &mut app, &mut work, &mut owner, 0)).await;
        }
        assert!(review_popup_ready(&mut owner, DecisionStatus::Pending));
        assert!(review_list(&app).contains("worker"));
        // A following agent.stuck event fences earlier task delivery on the real watcher.
        guarded(rig.org.execute(
            OrgCommand::Task(TaskCommand::Create(TaskCreated {
                project_id: Some("project".parse().unwrap()),
                title: "Concurrent review task".into(),
                kind: TaskKind::Work,
                creator: Address::Owner,
                parent: None,
                judges: None,
                measure: TaskMeasure::Owner,
                budget: None,
                files: vec![],
                rests_on: None,
            })),
            chrono::Utc::now(),
        ))
        .await
        .unwrap();
        let task_id = guarded(rig.org.snapshot())
            .await
            .unwrap()
            .tasks()
            .next()
            .unwrap()
            .id
            .clone();
        guarded(rig.org.execute(
            OrgCommand::Task(TaskCommand::Assign {
                task_id,
                agent_id: "project/worker".parse().unwrap(),
                by: TaskActor {
                    who: Address::Owner,
                    manager_of: None,
                },
            }),
            chrono::Utc::now(),
        ))
        .await
        .unwrap();
        guarded(rig.org.execute(
            OrgCommand::Stuck {
                agent_id: "project/worker".parse().unwrap(),
                reason: "review fence".into(),
            },
            chrono::Utc::now(),
        ))
        .await
        .unwrap();
        let mut task_events = Vec::new();
        loop {
            let kind = guarded(review_step(rig, &mut app, &mut work, &mut owner, 5)).await;
            if kind.starts_with("task.") {
                task_events.push(kind.clone());
            }
            if kind == "agent.stuck" {
                break;
            }
        }
        assert!(
            task_events.iter().any(|kind| kind == "task.created"),
            "task.created was lost while decision popup and Work were both open: {task_events:?}"
        );
        assert!(
            task_events.iter().any(|kind| kind == "task.assigned"),
            "task.assigned was lost: {task_events:?}"
        );
        while !review_list(&app).contains("Concurrent review task")
            || !review_popup_ready(&mut owner, DecisionStatus::Pending)
        {
            guarded(review_step(rig, &mut app, &mut work, &mut owner, 5)).await;
        }
        assert!(review_list(&app).contains('!'));
        assert_eq!(app.view, AppView::Work);
        guarded(rig.org.execute(
            OrgCommand::Decision(DecisionCommand::TakeProvisionally(ProvisionalChoice {
                decision_id: id,
                option: " yes ".into(),
                by: Address::System,
                relayed_by: None,
            })),
            chrono::Utc::now(),
        ))
        .await
        .unwrap();
        loop {
            if guarded(review_step(rig, &mut app, &mut work, &mut owner, 10)).await
                == "decision.taken_provisionally"
            {
                break;
            }
        }
        owner.key(key(KeyCode::Enter));
        assert!(
            rendered(owner.popup.as_mut().unwrap(), 100, 40)
                .contains("refreshing before answering"),
            "newer decision event failed to invalidate the simultaneously open popup"
        );
        while !review_popup_ready(&mut owner, DecisionStatus::TakenProvisionally)
            || !app
                .work
                .org()
                .unwrap()
                .needs_owner
                .items
                .iter()
                .any(|item| item.kind == "decision_provisional")
        {
            guarded(review_step(rig, &mut app, &mut work, &mut owner, 10)).await;
        }
        assert!(review_list(&app).contains("Concurrent review task"));
        if hide_first {
            let effects = app.toggle_work_view(Duration::from_secs(15));
            review_effects(&mut app, &mut work, &mut owner, effects);
            assert_eq!(app.view, AppView::Sessions);
            guarded(rig.org.execute(
                OrgCommand::Decision(DecisionCommand::Resolve(DecisionResolution {
                    decision_id: id,
                    by: Address::Owner,
                    option: " yes ".into(),
                    note: None,
                    standing_rule: false,
                })),
                chrono::Utc::now(),
            ))
            .await
            .unwrap();
            loop {
                if guarded(review_step(rig, &mut app, &mut work, &mut owner, 15)).await
                    == "decision.resolved"
                {
                    break;
                }
            }
            while !review_popup_ready(&mut owner, DecisionStatus::Resolved) {
                guarded(review_step(rig, &mut app, &mut work, &mut owner, 15)).await;
            }
            owner.key(key(KeyCode::Esc));
            assert!(owner.popup.is_none());
        } else {
            owner.key(key(KeyCode::Esc));
            assert!(owner.popup.is_none());
            let old_seq = app.work.org().unwrap().seq.unwrap();
            guarded(rig.org.execute(
                OrgCommand::Down {
                    agent_id: "project/worker".parse().unwrap(),
                    reason: "review close fence".into(),
                },
                chrono::Utc::now(),
            ))
            .await
            .unwrap();
            loop {
                if guarded(review_step(rig, &mut app, &mut work, &mut owner, 15)).await
                    == "agent.down"
                {
                    break;
                }
            }
            while app.work.org().unwrap().seq.unwrap() <= old_seq {
                guarded(review_step(rig, &mut app, &mut work, &mut owner, 15)).await;
            }
            assert!(owner.popup.is_none());
            assert_eq!(app.view, AppView::Work);
            assert!(
                !review_list(&app).contains("Concurrent review task"),
                "closed popup disabled Work's live refresh"
            );
            let effects = app.toggle_work_view(Duration::from_secs(15));
            review_effects(&mut app, &mut work, &mut owner, effects);
        }
        work.shutdown();
        owner.shutdown();
        eprintln!(
            "simultaneous real-daemon proof: work_first={work_first}, hide_first={hide_first}"
        );
    }
    async fn review_checked(work_first: bool, hide_first: bool) {
        let mut rig = ServerRig::start().await;
        let result = {
            let future = guarded(review_body(&mut rig, work_first, hide_first));
            tokio::pin!(future);
            std::future::poll_fn(|cx| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    future.as_mut().poll(cx)
                })) {
                    Ok(std::task::Poll::Ready(())) => std::task::Poll::Ready(Ok(())),
                    Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                    Err(error) => std::task::Poll::Ready(Err(error)),
                }
            })
            .await
        };
        rig.stop().await;
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
    #[tokio::test]
    async fn review_simultaneous_work_first_hide_first() {
        review_checked(true, true).await;
    }
    #[tokio::test]
    async fn review_simultaneous_work_first_popup_closes_first() {
        review_checked(true, false).await;
    }
    #[tokio::test]
    async fn review_simultaneous_popup_first_hide_first() {
        review_checked(false, true).await;
    }
    #[tokio::test]
    async fn review_simultaneous_popup_first_popup_closes_first() {
        review_checked(false, false).await;
    }

    // Neutral real committed frames, shared by both sides of the replay/draw measurement.
    pub(super) async fn recorded_work_usage_log(count: usize) -> Vec<DaemonMessage> {
        let mut rig = ServerRig::start().await;
        let result = {
            let future = async {
                let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
                guarded(rig.org.execute(
                    OrgCommand::Adopt {
                        agent_id: "project/worker".parse().unwrap(),
                        pane: "%7".into(),
                        session_id: Some("recorded-session".into()),
                    },
                    now,
                ))
                .await
                .unwrap();
                let cursor = rig.org.status().unwrap();
                let mut last = cursor;
                for _ in 0..count {
                    let events = guarded(rig.org.execute(
                        OrgCommand::ObserveTaskUsage(atm_org::TaskUsageObservation {
                            session_id: "recorded-session".into(),
                            pane: Some("%7".into()),
                            turn: true,
                            cumulative_cost: None,
                            context_percent: None,
                        }),
                        now,
                    ))
                    .await
                    .unwrap();
                    assert_eq!(events.len(), 1);
                    last = events.first().unwrap().seq;
                }
                rig.control.watch(vec!["task.".into()], cursor).unwrap();
                let mut frames = Vec::new();
                loop {
                    if let Event::Control(ControlNotice::Event(
                        frame @ DaemonMessage::Event { seq, .. },
                    )) = guarded(rig._events.recv()).await.unwrap()
                    {
                        frames.push(frame);
                        if seq == last {
                            break;
                        }
                    }
                }
                assert_eq!(frames.len(), count);
                frames
            };
            tokio::pin!(future);
            std::future::poll_fn(|cx| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    future.as_mut().poll(cx)
                })) {
                    Ok(std::task::Poll::Ready(frames)) => std::task::Poll::Ready(Ok(frames)),
                    Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                    Err(error) => std::task::Poll::Ready(Err(error)),
                }
            })
            .await
        };
        rig.stop().await;
        result.unwrap_or_else(|error| std::panic::resume_unwind(error))
    }

    // Characterization: real committed usage replays, and replacement is not additive.
    #[tokio::test]
    async fn recorded_usage_watch_replacement_is_single_scope() {
        let mut rig = ServerRig::start().await;
        let result = std::panic::AssertUnwindSafe(async {
            let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
            guarded(rig.org.execute(
                OrgCommand::Adopt {
                    agent_id: "project/worker".parse().unwrap(),
                    pane: "%7".into(),
                    session_id: Some("recorded-session".into()),
                },
                now,
            ))
            .await
            .unwrap();
            let cursor = rig.org.status().unwrap();
            let mut last = cursor;
            for _ in 0..16 {
                last = guarded(rig.org.execute(
                    OrgCommand::ObserveTaskUsage(atm_org::TaskUsageObservation {
                        session_id: "recorded-session".into(),
                        pane: Some("%7".into()),
                        turn: true,
                        cumulative_cost: None,
                        context_percent: None,
                    }),
                    now,
                ))
                .await
                .unwrap()
                .last()
                .unwrap()
                .seq;
            }
            rig.control.watch(vec!["task.".into()], cursor).unwrap();
            let mut count = 0;
            loop {
                if let Event::Control(ControlNotice::Event(DaemonMessage::Event {
                    seq,
                    kind,
                    ..
                })) = guarded(rig._events.recv()).await.unwrap()
                {
                    assert_eq!(kind, "task.usage_observed");
                    count += 1;
                    if seq == last {
                        break;
                    }
                }
            }
            assert_eq!(count, 16);
            rig.control.watch(vec!["decision.".into()], cursor).unwrap();
            let id = rig.request().await;
            loop {
                if let Event::Control(ControlNotice::Event(DaemonMessage::Event {
                    seq,
                    kind,
                    body,
                    ..
                })) = guarded(rig._events.recv()).await.unwrap()
                {
                    eprintln!("replacement event: seq={seq} kind={kind} body={body}");
                    assert_eq!(kind, "decision.requested");
                    assert_eq!(format!("d{seq}"), id.to_string());
                    break;
                }
            }
            eprintln!("real private atmd: 16 committed usage records replayed; replacement delivered decision only");
        });
        let result = {
            tokio::pin!(result);
            std::future::poll_fn(|cx| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    result.as_mut().poll(cx)
                })) {
                    Ok(std::task::Poll::Ready(())) => std::task::Poll::Ready(Ok(())),
                    Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                    Err(error) => std::task::Poll::Ready(Err(error)),
                }
            })
            .await
        };
        rig.stop().await;
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}

fn invalidation(seq: u64) -> atm_tui::control::ControlNotice {
    atm_tui::control::ControlNotice::Event(DaemonMessage::Event {
        topic: "org".into(),
        seq,
        ts: "2026-01-15T12:00:00Z".into(),
        kind: "decision.requested".into(),
        body: json!({}),
    })
}

// Break caught: capped org attention metadata truncates the queue or marks, or
// the watch starts at zero/current time instead of the read projection cursor.
#[tokio::test]
async fn marks_and_queue_use_all_pages_and_watch_the_snapshot_cursor() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.open_for(None);
    let mut org = tree();
    org["needs_owner"]["items"] = json!([]);
    org["needs_owner"]["items_total"] = json!(103);
    peer.reply("org.tree", org).await;
    peer.reply(
        "decision.list",
        page((1..=100).map(|id| entry(id, "system")).collect(), Some(100)),
    )
    .await;
    peer.reply(
        "decision.list",
        page(
            vec![
                entry(101, "alpha/lead"),
                entry(102, "alpha/watch"),
                entry(103, "system"),
            ],
            None,
        ),
    )
    .await;
    apply_next(&mut controller).await;
    peer.fence(&rig.commands).await;
    assert_eq!(
        controller.waiting_panes,
        std::collections::HashSet::from(["%7".to_owned()])
    );
    assert_eq!(
        peer.watches,
        vec![(
            vec!["decision.".into(), "agent.".into(), "project.".into()],
            41
        )]
    );
    for _ in 0..102 {
        controller.key(key(KeyCode::Down));
    }
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 14).contains("d103"));
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn watch_invalidations_coalesce_while_a_read_is_held_and_follow_new_bindings() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.refresh();
    let (held, _) = peer.request("org.tree").await;
    for seq in 42..=60 {
        controller.notice(&invalidation(seq));
    }
    // Legacy commands continue even with a held organisation read.
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_ok(held, tree())).await;
    peer.reply("decision.list", page(vec![entry(41, "alpha/lead")], None))
        .await;
    apply_next(&mut controller).await;
    let mut moved = tree();
    moved["seq"] = json!(60);
    moved["projects"][0]["seats"][0]["pane"] = json!("%8");
    peer.reply("org.tree", moved).await;
    peer.reply("decision.list", page(vec![entry(41, "alpha/lead")], None))
        .await;
    apply_next(&mut controller).await;
    peer.watch_at(60).await;
    peer.fence(&rig.commands).await;
    assert_eq!(
        controller.waiting_panes,
        std::collections::HashSet::from(["%8".to_owned()])
    );
    assert_eq!(peer.watches.last().unwrap().1, 60);
    controller.notice(&invalidation(59));
    peer.fence(&rig.commands).await; // replay already covered
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn watch_failure_disconnect_and_reconnect_refresh_reads_without_replaying_answers() {
    use atm_tui::control::{ControlFailure, ControlNotice};
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.refresh();
    peer.reply("org.tree", tree()).await;
    peer.reply("decision.list", page(vec![entry(41, "alpha/lead")], None))
        .await;
    apply_next(&mut controller).await;
    peer.fence(&rig.commands).await;
    assert!(!controller.waiting_panes.is_empty());
    controller.notice(&ControlNotice::WatchFailed(ControlFailure::Unsupported(
        "watch".into(),
    )));
    assert!(controller.waiting_panes.is_empty());
    peer.fence(&rig.commands).await;
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    controller.key(key(KeyCode::Enter));
    let (answer, _) = peer.request("decision.resolve").await;
    controller.disconnected();
    peer.send(DaemonMessage::response_err(
        answer,
        "unavailable",
        "connection lost after admission",
    ))
    .await;
    apply_next(&mut controller).await;
    controller.notice(&ControlNotice::Capabilities {
        protocol_version: ProtocolVersion::new(1, 1),
        methods: METHODS.iter().map(|m| (*m).into()).collect(),
    });
    let mut snapshot = tree();
    snapshot["seq"] = json!(50);
    peer.reply("org.tree", snapshot).await;
    peer.reply("decision.list", page(vec![], None)).await;
    apply_next(&mut controller).await;
    peer.fence(&rig.commands).await;
    assert!(controller.waiting_panes.is_empty());
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
    controller.key(key(KeyCode::Enter));
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

// Break caught: a dead watch leaves a pending popup able to overrule without [y/N].
#[tokio::test]
async fn watch_failed_popup_rechecks_before_enter_and_coalesces_bursts() {
    use atm_tui::control::{ControlFailure, ControlNotice};
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    controller.key(key(KeyCode::Char('j'))); // explicit No
    let failed = ControlNotice::WatchFailed(ControlFailure::Timeout);
    controller.notice(&failed);
    controller.key(key(KeyCode::Enter));
    let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
    if text.contains("Recording answer") {
        let (_, params) = peer.request("decision.resolve").await;
        assert_eq!(
            params,
            json!({"id":"d41","option":"No","note":null,"standing_rule":false})
        );
        panic!("unconfirmed WatchFailed mutation on wire: {params}");
    }
    assert!(text.contains("refreshing before answering"), "{text}");
    let (held, params) = peer.request("decision.get").await;
    assert_eq!(params["id"], "d41");
    for _ in 0..8 {
        controller.notice(&failed);
    }
    peer.fence(&rig.commands).await; // no concurrent replacement, snapshot or re-watch loop
    peer.send(DaemonMessage::response_ok(
        held,
        json!({"decision":entry(41,"alpha/lead")}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Enter));
    let (fresh, _) = peer.request("decision.get").await;
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_ok(
        fresh,
        json!({"decision":provisional_entry()}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Enter));
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("[y/N]"));
    peer.fence(&rig.commands).await;
    controller.key(key(KeyCode::Enter)); // default No cancels, not approval
    peer.fence(&rig.commands).await;
    controller.key(key(KeyCode::Enter));
    controller.key(key(KeyCode::Char('y')));
    let (answer, params) = peer.request("decision.resolve").await;
    assert_eq!(
        params,
        json!({"id":"d41","option":"No","note":null,"standing_rule":false})
    );
    controller.notice(&failed); // must not replace the mutation with a read
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_err(
        answer,
        "unavailable",
        "result lost after admission",
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.notice(&failed); // unknown outcomes stay gated, not silently reloaded/replayed
    controller.key(key(KeyCode::Enter));
    peer.fence(&rig.commands).await;
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
    controller.shutdown();
    rig.stop().await;
}

// Break caught: WatchFailed observed while Loading is forgotten by an older get completion.
#[tokio::test]
async fn watch_failed_during_initial_get_requires_one_post_failure_get() {
    use atm_tui::control::{ControlFailure, ControlNotice};
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.open(atm_org::DecisionId::new(41));
    let (held, _) = peer.request("decision.get").await;
    for _ in 0..8 {
        controller.notice(&ControlNotice::WatchFailed(ControlFailure::Disconnected));
    }
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_ok(
        held,
        json!({"decision":entry(41,"alpha/lead")}),
    ))
    .await;
    apply_popup(&mut controller).await;
    assert!(matches!(
        controller.popup.as_ref().unwrap().screen,
        Screen::Loading
    ));
    controller.key(key(KeyCode::Char('k')));
    controller.key(key(KeyCode::Enter));
    let (_, params) = peer.request("decision.get").await;
    assert_eq!(params["id"], "d41");
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn stale_popup_reply_cannot_replace_a_newer_selection() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.open(atm_org::DecisionId::new(41));
    let (old, _) = peer.request("decision.get").await;
    controller.open(atm_org::DecisionId::new(42));
    let (new, _) = peer.request("decision.get").await;
    peer.send(DaemonMessage::response_ok(
        new,
        json!({"decision":entry(42,"beta/lead")}),
    ))
    .await;
    apply_next(&mut controller).await;
    peer.send(DaemonMessage::response_ok(
        old,
        json!({"decision":entry(41,"alpha/lead")}),
    ))
    .await;
    peer.fence(&rig.commands).await;
    let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
    assert!(text.contains("d42 from beta/lead") && !text.contains("d41 from alpha/lead"));
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn missing_snapshot_cursor_is_unavailable_not_an_empty_queue() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.open_for(None);
    let mut org = tree();
    org.as_object_mut().unwrap().remove("seq");
    peer.reply("org.tree", org).await;
    // A missing cursor is rejected before asking for a list or installing a watch.
    apply_next(&mut controller).await;
    peer.fence(&rig.commands).await;
    assert!(matches!(
        controller.popup.as_ref().unwrap().screen,
        Screen::Error(_)
    ));
    assert!(peer.watches.is_empty() && controller.waiting_panes.is_empty());
    controller.shutdown();
    rig.stop().await;
}

// Break caught: a chooser Enter followed by another Press records a default answer.
#[test]
fn review_chooser_enter_then_loaded_then_enter_never_records_an_unselected_answer() {
    let mut view = DecisionPopup::default();
    view.reduce(Input::Queue(vec![entry(41, "alpha/lead")]));
    assert!(matches!(
        view.reduce(Input::Key(key(KeyCode::Enter))).as_slice(),
        [Effect::Get(_)]
    ));
    view.reduce(Input::Loaded(Box::new(entry(41, "alpha/lead"))));
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    let text = rendered(&mut view, 100, 40);
    assert!(text.contains("Selected: none"), "{text}");
    view.reduce(Input::Key(key(KeyCode::Char('k'))));
    let effects = view.reduce(Input::Key(key(KeyCode::Enter)));
    assert!(matches!(effects.as_slice(), [Effect::Resolve(p)] if p.option == " yes "));
}

// Break caught: a late direct-open/reinspect get switches a chooser under Enter.
#[test]
fn review_loaded_reply_never_replaces_a_showing_chooser() {
    let mut direct = DecisionPopup::default();
    direct.reduce(Input::Queue(vec![entry(41, "alpha/lead")]));
    direct.reduce(Input::Loaded(Box::new(entry(41, "alpha/lead"))));
    assert!(matches!(direct.screen, Screen::Queue));
    assert!(matches!(
        direct.reduce(Input::Key(key(KeyCode::Enter))).as_slice(),
        [Effect::Get(_)]
    ));

    let mut view = DecisionPopup::default();
    view.reduce(Input::Queue(vec![
        entry(41, "alpha/lead"),
        entry(42, "beta/lead"),
    ]));
    view.reduce(Input::Key(key(KeyCode::Enter)));
    view.reduce(Input::Loaded(Box::new(entry(41, "alpha/lead"))));
    view.reduce(Input::Key(key(KeyCode::Char('i'))));
    view.reduce(Input::Key(key(KeyCode::Esc)));
    view.reduce(Input::Loaded(Box::new(entry(41, "alpha/lead"))));
    assert!(matches!(view.screen, Screen::Queue));
    assert!(matches!(
        view.reduce(Input::Key(key(KeyCode::Enter))).as_slice(),
        [Effect::Get(_)]
    ));
}

// Serve read-only refreshes until the client sends an ordered Discover fence.
async fn serve_popup_refreshes(mut peer: Peer, provisional: DecisionEntry, cursor: u64) -> Peer {
    loop {
        match peer.recv().await {
            MessageType::Discover => return peer,
            MessageType::Request {
                id, method, params, ..
            } => {
                let result = match method.as_str() {
                    "org.tree" => {
                        let mut value = tree();
                        value["seq"] = json!(cursor);
                        value
                    }
                    "decision.list" => page(vec![provisional.clone()], None),
                    "decision.get" => {
                        assert_eq!(params["id"], "d41");
                        json!({"decision":provisional})
                    }
                    _ => panic!("unexpected request while refreshing: {method}: {params}"),
                };
                peer.send(DaemonMessage::response_ok(id, result)).await;
            }
            other => panic!("unexpected refresh message: {other:?}"),
        }
    }
}
async fn apply_current_popup(controller: &mut DecisionController) {
    // The initial snapshot and get can finish in either order. A snapshot that
    // finishes second invalidates that get and requires a coalesced follow-up.
    for _ in 0..2 {
        apply_next(controller).await;
    }
    while matches!(controller.popup.as_ref().unwrap().screen, Screen::Loading)
        || rendered(controller.popup.as_mut().unwrap(), 100, 40)
            .contains("refreshing before answering")
    {
        apply_next(controller).await;
    }
}

// Break caught: a matching decision notice leaves the stale pending status answerable.
#[tokio::test]
async fn review_provisional_notice_refreshes_open_draft_before_overrule() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    controller.key(key(KeyCode::Char('k')));
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Tab));
    controller.key(key(KeyCode::Char('x')));
    controller.key(key(KeyCode::Esc));
    let mut provisional = entry(41, "alpha/lead");
    provisional.decision.status = atm_org::DecisionStatus::TakenProvisionally;
    provisional.decision.provisional = Some(atm_org::ProvisionalChoice {
        decision_id: provisional.decision.id,
        option: " yes ".into(),
        by: atm_org::Address::System,
        relayed_by: None,
    });
    let notice = atm_tui::control::ControlNotice::Event(DaemonMessage::Event {
        topic: "org".into(),
        seq: 42,
        ts: "2026-01-15T12:00:00Z".into(),
        kind: "decision.taken_provisionally".into(),
        body: json!({"decision_id":"d41","option":" yes ","by":"system"}),
    });
    controller.notice(&notice);
    // An Enter before the fresh reply must not send a mutation either.
    controller.key(key(KeyCode::Enter));
    assert!(!rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Recording answer"));
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut reads = Vec::new();
    loop {
        match peer.recv().await {
            MessageType::Request { id, method, .. } => {
                assert!(
                    matches!(method.as_str(), "org.tree" | "decision.get"),
                    "stale submission: {method}"
                );
                reads.push((id, method));
            }
            MessageType::Discover => break,
            other => panic!("unexpected message: {other:?}"),
        }
    }
    let server = tokio::spawn(async move {
        for (id, method) in reads {
            let result = match method.as_str() {
                "org.tree" => {
                    let mut value = tree();
                    value["seq"] = json!(42);
                    value
                }
                "decision.get" => json!({"decision":provisional}),
                _ => panic!("unexpected initial refresh: {method}"),
            };
            peer.send(DaemonMessage::response_ok(id, result)).await;
        }
        serve_popup_refreshes(peer, provisional, 42).await
    });
    apply_current_popup(&mut controller).await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = guarded(server).await.unwrap();
    controller.key(key(KeyCode::Enter));
    let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
    assert!(
        text.contains("[y/N]") && text.contains("\"No\"") && text.contains("Note (optional): x"),
        "{text}"
    );
    peer.fence(&rig.commands).await;
    controller.key(key(KeyCode::Char('y')));
    let (_, params) = peer.request("decision.resolve").await;
    assert_eq!(params["option"], "No");
    assert_eq!(params["note"], "x");
    // The same event is covered by the snapshot; no read may replace a mutation.
    controller.notice(&notice);
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

// Break caught: Enter from a multiline paste leaves note focus and runs j/y as commands.
#[test]
fn review_multiline_note_keys_only_change_the_note() {
    let mut view = popup(entry(41, "alpha/lead"));
    view.reduce(Input::Key(key(KeyCode::Char('k'))));
    view.reduce(Input::Key(key(KeyCode::Tab)));
    for code in [
        KeyCode::Char('a'),
        KeyCode::Char('b'),
        KeyCode::Char('c'),
        KeyCode::Enter,
        KeyCode::Char('j'),
        KeyCode::Char('y'),
        KeyCode::Enter,
    ] {
        assert!(view.reduce(Input::Key(key(code))).is_empty());
    }
    let text = rendered(&mut view, 100, 40);
    assert!(
        text.contains("abcjy") && text.contains("Esc finishes"),
        "{text}"
    );
    view.reduce(Input::Key(key(KeyCode::Esc)));
    let effects = view.reduce(Input::Key(key(KeyCode::Enter)));
    assert!(matches!(effects.as_slice(), [Effect::Resolve(p)]
        if p.option == " yes " && p.note.as_deref() == Some("abcjy")));
}

// Break caught: a failed read before first connection is never restarted on capabilities,
// including when its failure update is still queued as capabilities arrive.
#[tokio::test(start_paused = true)]
async fn review_late_capabilities_retry_a_failed_startup_read() {
    for capabilities_first in [false, true] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut controller = DecisionController::new(ControlHandle::new(tx), None);
        controller.refresh();
        let _held = rx.recv().await.unwrap();
        tokio::time::advance(Duration::from_secs(17)).await;
        let notice = atm_tui::control::ControlNotice::Capabilities {
            protocol_version: ProtocolVersion::new(1, 1),
            methods: METHODS.iter().map(|m| (*m).into()).collect(),
        };
        if capabilities_first {
            controller.notice(&notice);
        }
        apply_next(&mut controller).await;
        assert!(controller.waiting_panes.is_empty());
        if !capabilities_first {
            controller.notice(&notice);
        }
        assert!(matches!(
            guarded(rx.recv()).await,
            Some(ClientCommand::Control(_))
        ));
        controller.shutdown();
    }
}

// Break caught: a pasted Tab exits note focus, selects an option and sends an answer.
async fn note_paste_only_edits_text(keys: &[KeyEvent], note: &str) {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.open(atm_org::DecisionId::new(41));
    peer.reply("decision.get", json!({"decision":entry(41,"alpha/lead")}))
        .await;
    apply_next(&mut controller).await;
    controller.key(key(KeyCode::Tab));
    for event in keys {
        controller.key(*event);
    }
    let text = rendered(controller.popup.as_mut().unwrap(), 120, 40);
    assert!(
        text.contains("Selected: none"),
        "paste selected an option: {text}"
    );
    assert!(text.contains(&format!("ignored): {note}")), "{text}");
    peer.fence(&rig.commands).await; // no resolve request from the paste
    controller.key(key(KeyCode::Esc)); // only an explicit Escape leaves note editing
    controller.key(key(KeyCode::Enter));
    peer.fence(&rig.commands).await; // no choice was made by any pasted key
    controller.key(key(KeyCode::Char('k')));
    controller.key(key(KeyCode::Enter));
    let (_, params) = peer.request("decision.resolve").await;
    assert_eq!(params["option"], " yes ");
    assert_eq!(params["note"], note);
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn round2_note_pasted_tab_then_option_and_enter_never_submits() {
    note_paste_only_edits_text(
        &[
            key(KeyCode::Char('a')),
            key(KeyCode::Tab),
            key(KeyCode::Char('k')),
            key(KeyCode::Enter),
        ],
        "ak",
    )
    .await;
}

#[tokio::test]
async fn round2_tab_separated_note_paste_with_line_feed_is_only_text() {
    note_paste_only_edits_text(
        &[
            key(KeyCode::Char('s')),
            key(KeyCode::Char('e')),
            key(KeyCode::Char('e')),
            key(KeyCode::Tab),
            key(KeyCode::Char('o')),
            key(KeyCode::Char('k')),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL), // raw LF
        ],
        "seeok",
    )
    .await;
}

// Break caught: events missed during a real transport reconnect leave a pending
// popup answerable without confirming a newly provisional overrule.
#[tokio::test]
async fn round2_reconnected_popup_refreshes_before_a_provisional_overrule() {
    use atm_tui::control::ControlNotice;
    let (mut rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.refresh();
    peer.reply("org.tree", tree()).await;
    peer.reply("decision.list", page(vec![entry(41, "alpha/lead")], None))
        .await;
    apply_next(&mut controller).await;
    peer.watch_at(41).await;
    // Consume the first opt-in capabilities before opening the popup.
    loop {
        if let Event::Control(notice @ ControlNotice::Capabilities { .. }) =
            guarded(rig._events.recv()).await.unwrap()
        {
            controller.notice(&notice);
            break;
        }
    }
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    controller.key(key(KeyCode::Char('j'))); // explicit No, recommendation was yes
    let path = rig._dir.path().join("peer.sock");
    tokio::fs::remove_file(&path).await.unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    drop(peer);
    loop {
        if matches!(
            guarded(rig._events.recv()).await.unwrap(),
            Event::DaemonDisconnected(_)
        ) {
            controller.disconnected();
            break;
        }
    }
    let (stream, _) = guarded(listener.accept()).await.unwrap();
    let (reader, writer) = stream.into_split();
    let mut peer = Peer {
        reader: BufReader::new(reader),
        writer,
        watches: Vec::new(),
    };
    assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
    peer.send(DaemonMessage::Connected {
        protocol_version: ProtocolVersion::new(1, 1),
        client_id: "reconnected-peer".into(),
        methods: METHODS.iter().map(|m| (*m).into()).collect(),
    })
    .await;
    assert!(matches!(peer.recv().await, MessageType::Subscribe { .. }));
    peer.send(DaemonMessage::session_list(vec![])).await;
    let capabilities = loop {
        if let Event::Control(notice @ ControlNotice::Capabilities { .. }) =
            guarded(rig._events.recv()).await.unwrap()
        {
            break notice;
        }
    };
    controller.notice(&capabilities);
    controller.key(key(KeyCode::Enter)); // fresh get remains held
    assert!(!rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Recording answer"));
    let mut provisional = entry(41, "alpha/lead");
    provisional.decision.status = atm_org::DecisionStatus::TakenProvisionally;
    provisional.decision.provisional = Some(atm_org::ProvisionalChoice {
        decision_id: provisional.decision.id,
        option: " yes ".into(),
        by: atm_org::Address::System,
        relayed_by: None,
    });
    let server = tokio::spawn(serve_popup_refreshes(peer, provisional, 50));
    apply_current_popup(&mut controller).await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = guarded(server).await.unwrap();
    controller.key(key(KeyCode::Enter));
    let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
    assert!(
        text.contains("[y/N]") && text.contains("Selected: \"No\""),
        "{text}"
    );
    peer.fence(&rig.commands).await;
    controller.key(key(KeyCode::Enter)); // default No cancels, not approval
    peer.fence(&rig.commands).await;
    controller.key(key(KeyCode::Enter));
    controller.key(key(KeyCode::Char('y')));
    let (_, params) = peer.request("decision.resolve").await;
    assert_eq!(params["option"], "No");
    controller.notice(&capabilities); // a read must not replace this in-flight mutation
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

// Adopted from the independent review: exact outgoing params expose a stale overrule.
#[tokio::test]
async fn round2_independent_reconnect_must_refresh_open_status_before_overrule() {
    use atm_tui::control::ControlNotice;
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Tab));
    controller.key(key(KeyCode::Char('x')));
    controller.key(key(KeyCode::Esc));
    controller.disconnected();
    controller.notice(&ControlNotice::Capabilities {
        protocol_version: ProtocolVersion::new(1, 1),
        methods: METHODS.iter().map(|m| (*m).into()).collect(),
    });
    loop {
        let MessageType::Request {
            id, method, params, ..
        } = peer.recv().await
        else {
            panic!("expected reconnect read")
        };
        match method.as_str() {
            "org.tree" => {
                let mut org = tree();
                org["seq"] = json!(50);
                peer.send(DaemonMessage::response_ok(id, org)).await;
            }
            "decision.list" => {
                peer.send(DaemonMessage::response_ok(
                    id,
                    page(vec![provisional_entry()], None),
                ))
                .await;
                break;
            }
            "decision.get" => assert_eq!(params["id"], "d41"), // deliberately held
            _ => panic!("unexpected reconnect request: {method}: {params}"),
        }
    }
    apply_next(&mut controller).await; // fresh seq-50 snapshot/list, watch starts at 50
    controller.key(key(KeyCode::Enter));
    let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
    if text.contains("Recording answer") {
        let (_, params) = peer.request("decision.resolve").await;
        assert_eq!(
            params,
            json!({"id":"d41","option":"No","note":"x","standing_rule":false})
        );
        panic!("unconfirmed reconnect mutation on wire: {params}");
    }
    assert!(
        text.contains("[y/N]") || text.contains("refreshing before answering"),
        "{text}"
    );
    controller.shutdown();
    rig.stop().await;
}

fn provisional_entry() -> DecisionEntry {
    let mut item = entry(41, "alpha/lead");
    item.decision.status = atm_org::DecisionStatus::TakenProvisionally;
    item.decision.provisional = Some(atm_org::ProvisionalChoice {
        relayed_by: None,
        decision_id: item.decision.id,
        option: " yes ".into(),
        by: atm_org::Address::System,
    });
    item
}

#[tokio::test]
async fn round2_independent_decision_notice_during_get_must_not_be_lost() {
    use atm_tui::control::ControlNotice;
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.refresh();
    peer.reply("org.tree", tree()).await;
    peer.reply("decision.list", page(vec![entry(41, "alpha/lead")], None))
        .await;
    apply_next(&mut controller).await;
    peer.watch_at(41).await;
    controller.open(atm_org::DecisionId::new(41));
    let (held, _) = peer.request("decision.get").await;
    let notice = ControlNotice::Event(DaemonMessage::Event {
        topic: "org".into(),
        seq: 42,
        ts: "2026-01-15T12:00:00Z".into(),
        kind: "decision.taken_provisionally".into(),
        body: json!({"decision_id":"d41","option":" yes ","by":"system"}),
    });
    controller.notice(&notice);
    let (held_tree, _) = peer.request("org.tree").await;
    peer.fence(&rig.commands).await; // no replacement get while the older one is held
    peer.send(DaemonMessage::response_ok(
        held,
        json!({"decision":entry(41,"alpha/lead")}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Char('k')));
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Enter));
    let text = rendered(controller.popup.as_mut().unwrap(), 100, 40);
    if text.contains("Recording answer") {
        loop {
            let MessageType::Request { method, params, .. } = peer.recv().await else {
                panic!("expected read or mutation")
            };
            if method == "decision.resolve" {
                assert_eq!(
                    params,
                    json!({"id":"d41","option":"No","note":null,"standing_rule":false})
                );
                panic!("unconfirmed post-event mutation on wire: {params}");
            }
            assert_eq!(method, "org.tree");
        }
    }
    assert!(!text.contains("Recording answer"), "{text}");
    let (after_notice, _) = peer.request("decision.get").await;
    let mut org = tree();
    org["seq"] = json!(42);
    peer.send(DaemonMessage::response_ok(held_tree, org)).await;
    peer.reply("decision.list", page(vec![provisional_entry()], None))
        .await;
    apply_next(&mut controller).await; // fresh snapshot invalidates the held get too
    for _ in 0..8 {
        controller.notice(&notice); // covered by cursor: no new projection read
        controller.notice(&ControlNotice::Capabilities {
            protocol_version: ProtocolVersion::new(1, 1),
            methods: METHODS.iter().map(|m| (*m).into()).collect(),
        });
    }
    peer.fence(&rig.commands).await; // bursts coalesce; still one get in flight
    peer.send(DaemonMessage::response_ok(
        after_notice,
        json!({"decision":entry(41,"alpha/lead")}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Enter));
    assert!(!rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Recording answer"));
    peer.reply("decision.get", json!({"decision":provisional_entry()}))
        .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Char('k')));
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Enter));
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("[y/N]"));
    peer.fence(&rig.commands).await; // no extra get/spin and no unconfirmed resolve
    controller.key(key(KeyCode::Char('y')));
    let (_, params) = peer.request("decision.resolve").await;
    assert_eq!(
        params,
        json!({"id":"d41","option":"No","note":null,"standing_rule":false})
    );
    controller.shutdown();
    rig.stop().await;
}

#[test]
fn round2_independent_multiline_tabbed_paste_must_not_record() {
    let mut view = popup(entry(41, "alpha/lead"));
    view.reduce(Input::Key(key(KeyCode::Char('k'))));
    view.reduce(Input::Key(key(KeyCode::Tab)));
    let mut effects = Vec::new();
    for code in [
        KeyCode::Char('a'),
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::Char('j'),
        KeyCode::Enter,
    ] {
        effects.extend(view.reduce(Input::Key(key(code))));
    }
    assert!(effects.is_empty(), "tabbed paste ran commands: {effects:?}");
    view.reduce(Input::Key(key(KeyCode::Esc)));
    let effects = view.reduce(Input::Key(key(KeyCode::Enter)));
    assert!(
        matches!(effects.as_slice(), [Effect::Resolve(p)] if p.option == " yes " && p.note.as_deref() == Some("aj"))
    );
}

const METHODS: &[&str] = &[
    "org.tree",
    "decision.list",
    "decision.get",
    "decision.resolve",
];

// Break caught: selected-row placement is guessed locally, uses a subagent's shared pane,
// or silently truncates the authorized queue at the first page / capped org metadata.
#[tokio::test]
async fn selected_sole_request_opens_fresh_and_multiple_requests_choose() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.open_for(Some(&selected("%7")));
    peer.reply("org.tree", tree()).await;
    let params = peer
        .reply(
            "decision.list",
            page(vec![entry(41, "alpha/lead")], Some(41)),
        )
        .await;
    assert_eq!(params["after"], Value::Null);
    let params = peer
        .reply("decision.list", page(vec![entry(42, "beta/lead")], None))
        .await;
    assert_eq!(params["after"], "d41");
    apply_next(&mut controller).await;
    assert!(matches!(
        controller.popup.as_ref().unwrap().screen,
        Screen::Loading
    ));
    let params = peer
        .reply("decision.get", json!({"decision":entry(41,"alpha/lead")}))
        .await;
    assert_eq!(params["id"], "d41");
    apply_next(&mut controller).await;
    assert!(matches!(
        controller.popup.as_ref().unwrap().screen,
        Screen::Inspect(_)
    ));
    controller.open_for(Some(&selected("%7")));
    peer.reply("org.tree", tree()).await;
    peer.reply(
        "decision.list",
        page(
            vec![
                entry(41, "alpha/lead"),
                entry(42, "beta/lead"),
                entry(43, "alpha/lead"),
            ],
            None,
        ),
    )
    .await;
    apply_next(&mut controller).await;
    let text = rendered(controller.popup.as_mut().unwrap(), 80, 16);
    assert!(text.contains("d41") && text.contains("d43"));
    assert!(!text.contains("beta/lead"));
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn group_unmapped_and_shared_pane_child_open_global_queue() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    let mut child = selected("%7");
    child.parent_session_id = Some("parent".into());
    for session in [None, Some(selected("%404")), Some(child)] {
        controller.open_for(session.as_ref());
        peer.reply("org.tree", tree()).await;
        peer.reply(
            "decision.list",
            page(vec![entry(41, "alpha/lead"), entry(42, "system")], None),
        )
        .await;
        apply_next(&mut controller).await;
        let text = rendered(controller.popup.as_mut().unwrap(), 80, 16);
        assert!(text.contains("alpha/lead") && text.contains("system"));
    }
    controller.shutdown();
    rig.stop().await;
}

fn relayed_entry(id: u64, requester: &str, seat: &str) -> DecisionEntry {
    let mut entry = entry(id, requester);
    entry.decision.status = atm_org::DecisionStatus::TakenProvisionally;
    entry.decision.provisional = Some(atm_org::ProvisionalChoice {
        decision_id: entry.decision.id,
        option: " yes ".into(),
        by: atm_org::Address::Owner,
        relayed_by: Some(seat.parse().unwrap()),
    });
    entry
}

#[test]
fn decision_relayed_chooser_snapshot() {
    let mut chooser = DecisionPopup::default();
    chooser.reduce(Input::Queue(vec![
        relayed_entry(41, "demo/builder", "demo/chief"),
        entry(42, "demo/chief"),
    ]));
    insta::assert_snapshot!("decision_relayed_chooser", rendered(&mut chooser, 80, 14));
}

#[test]
fn decision_relayed_inspect_snapshot() {
    insta::assert_snapshot!(
        "decision_relayed_inspect",
        rendered(
            &mut popup(relayed_entry(41, "demo/builder", "demo/chief")),
            110,
            36
        )
    );
}

// Break caught: provenance is hidden, a self-relay looks agent-decided, or final decisions claim no confirmation.
#[test]
fn relay_marks_only_live_provisional_queue_and_inspection() {
    for requester in ["alpha/build", "alpha/chief"] {
        let item = relayed_entry(41, requester, "alpha/chief");
        let text = rendered(&mut popup(item.clone()), 110, 40);
        assert!(
            text.contains("Relayed by alpha/chief as your spoken answer; not confirmed by you"),
            "{text}"
        );
        let mut chooser = DecisionPopup::default();
        chooser.reduce(Input::Queue(vec![item.clone(), entry(42, "beta/lead")]));
        let text = rendered(&mut chooser, 110, 14);
        assert_eq!(text.matches("[relayed]").count(), 1);
        for status in [
            atm_org::DecisionStatus::Resolved,
            atm_org::DecisionStatus::Overruled,
            atm_org::DecisionStatus::Withdrawn,
            atm_org::DecisionStatus::Expired,
        ] {
            let mut final_entry = item.clone();
            final_entry.decision.status = status;
            assert!(!rendered(&mut popup(final_entry.clone()), 110, 40)
                .contains("not confirmed by you"));
            chooser.reduce(Input::Queue(vec![final_entry]));
            assert!(!rendered(&mut chooser, 110, 14).contains("[relayed]"));
        }
        let mut legacy = item;
        legacy.decision.provisional.as_mut().unwrap().relayed_by = None;
        assert!(!rendered(&mut popup(legacy.clone()), 110, 40).contains("Relayed by"));
        chooser.reduce(Input::Queue(vec![legacy]));
        assert!(!rendered(&mut chooser, 110, 14).contains("[relayed]"));
    }
}

// Break caught: displaying a relay preselects/auto-submits it or bypasses the ordinary overrule confirmation.
#[test]
fn relay_does_not_preselect_or_change_same_option_or_overrule_keys() {
    let entry = relayed_entry(41, "alpha/build", "alpha/chief");
    let mut view = popup(entry.clone());
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    view.reduce(Input::Key(key(KeyCode::Char('j'))));
    let effects = view.reduce(Input::Key(key(KeyCode::Enter)));
    let [Effect::Resolve(params)] = effects.as_slice() else {
        panic!("one ordinary confirmation")
    };
    assert_eq!(
        serde_json::to_value(params).unwrap(),
        json!({"id":"d41","option":" yes ","note":null,"standing_rule":false})
    );
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    let mut view = popup(entry);
    for _ in 0..2 {
        view.reduce(Input::Key(key(KeyCode::Char('j'))));
    }
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    assert!(rendered(&mut view, 110, 40).contains("Confirm d41 answer \"No\"? [y/N]"));
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    assert!(view.reduce(Input::Key(key(KeyCode::Enter))).is_empty());
    let effects = view.reduce(Input::Key(key(KeyCode::Char('y'))));
    let [Effect::Resolve(params)] = effects.as_slice() else {
        panic!("one explicit overrule")
    };
    assert_eq!(params.option, "No");
}

// Break caught: added wrapped markers are omitted from preceding rows' scroll offsets.
#[test]
fn relay_marker_wrap_is_included_in_chooser_selection_scroll() {
    let entries = (1..=20)
        .map(|id| {
            let mut entry = relayed_entry(id, "alpha/lead", "alpha/chief");
            entry.decision.request.title = format!("q{id:02}🙂");
            entry
        })
        .collect();
    let mut chooser = DecisionPopup::default();
    chooser.reduce(Input::Queue(entries));
    for _ in 0..20 {
        chooser.reduce(Input::Key(key(KeyCode::Char('j'))));
    }
    let text = rendered(&mut chooser, 32, 10);
    assert!(text.contains("[relayed]"), "{text}");
    assert!(text.contains("> d20 from alpha/lead: q20"), "{text}");
    let wider = rendered(&mut chooser, 110, 14);
    // The buffer dump retains the emoji's second, blank continuation cell.
    assert!(
        wider.contains("> d20 from alpha/lead: q20🙂  [relayed]"),
        "{wider}"
    );
}

fn work_interest(cursor: u64) -> (u64, Vec<String>) {
    (
        cursor,
        vec![
            "task.".into(),
            "decision.".into(),
            "agent.".into(),
            "msg.delivery_failed".into(),
        ],
    )
}

async fn work_next_watch(peer: &mut Peer) {
    loop {
        match peer.raw().await {
            MessageType::Watch { topics, cursor } => {
                peer.watches.push((topics, cursor));
                return;
            }
            MessageType::Unwatch => {}
            MessageType::Ping { seq } => peer.send(DaemonMessage::Pong { seq }).await,
            other => panic!("expected the completed watch barrier, got {other:?}"),
        }
    }
}

async fn work_snapshot(controller: &mut DecisionController, peer: &mut Peer, seq: u64) {
    controller.refresh();
    let mut org = tree();
    org["seq"] = json!(seq);
    peer.reply("org.tree", org).await;
    peer.reply("decision.list", page(vec![entry(41, "alpha/lead")], None))
        .await;
    apply_next(controller).await;
    work_next_watch(peer).await;
}

async fn work_event(rig: &mut Rig, peer: &mut Peer, seq: u64) -> atm_tui::control::ControlNotice {
    peer.watch_at(30).await;
    peer.send(DaemonMessage::Event {
        topic: "org".into(),
        seq,
        ts: "2026-01-15T12:00:00Z".into(),
        kind: "decision.taken_provisionally".into(),
        body: json!({"decision_id":"d41"}),
    })
    .await;
    loop {
        if let Event::Control(
            notice @ atm_tui::control::ControlNotice::Event(DaemonMessage::Event {
                seq: got, ..
            }),
        ) = guarded(rig._events.recv()).await.unwrap()
        {
            assert_eq!(got, seq);
            return notice;
        }
    }
}

// A join must start a new projection before replacing the old physical watch.
async fn work_wire_fence(rig: &mut Rig, peer: &mut Peer) {
    peer.fence(&rig.commands).await;
    // Discover fences commands, not a still-pending Pong. This ordered inbound
    // frame proves the client processed that Pong before we fence outgoing Watch.
    peer.send(DaemonMessage::session_list(vec![])).await;
    while !matches!(
        guarded(rig._events.recv()).await.unwrap(),
        Event::SessionListReplace(_)
    ) {}
    peer.fence(&rig.commands).await;
}
async fn work_join_org(peer: &mut Peer) -> String {
    match peer.raw().await {
        MessageType::Request {
            id,
            method,
            from_pane,
            ..
        } if method == "org.tree" => {
            assert_eq!(from_pane.as_deref(), Some("%99"));
            id
        }
        other => panic!("Work installed a stale union before a post-join snapshot: {other:?}"),
    }
}
async fn work_finish_join(controller: &mut DecisionController, peer: &mut Peer, seq: u64) {
    let id = work_join_org(peer).await;
    let mut org = tree();
    org["seq"] = json!(seq);
    peer.send(DaemonMessage::response_ok(id, org)).await;
    peer.reply("decision.list", page(vec![entry(41, "alpha/lead")], None))
        .await;
    apply_next(controller).await;
    work_next_watch(peer).await;
}

// Break caught: an old decision floor adds thousands of already-covered task frames per w.
#[tokio::test]
async fn work_join_waits_for_post_join_snapshot_without_replaying_covered_tasks() {
    let (mut rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    let recorded: Vec<_> = (42..=2089)
        .map(|seq| DaemonMessage::Event {
            topic: "task".into(),
            seq,
            ts: "2026-01-15T12:00:00Z".into(),
            kind: "task.usage_observed".into(),
            body: json!({}),
        })
        .collect();
    for fresh in [1065, 2089] {
        controller
            .set_work_watch(Some(work_interest(fresh)))
            .unwrap();
        work_finish_join(&mut controller, &mut peer, fresh).await;
        let (topics, cursor) = peer.watches.last().unwrap();
        assert_eq!(*cursor, fresh);
        assert!(topics.iter().any(|topic| topic == "task."));
        let cursor = *cursor;
        for event in &recorded {
            if matches!(event, DaemonMessage::Event {seq, ..} if *seq > cursor && *seq <= fresh) {
                peer.send(event.clone()).await;
            }
        }
        // This frame fences every replay write on the same socket and event channel.
        peer.send(DaemonMessage::session_list(vec![])).await;
        let mut replayed = 0;
        loop {
            match guarded(rig._events.recv()).await.unwrap() {
                Event::Control(atm_tui::control::ControlNotice::Event(DaemonMessage::Event {
                    kind,
                    ..
                })) if kind.starts_with("task.") => replayed += 1,
                Event::SessionListReplace(_) => break,
                _ => {}
            }
        }
        assert_eq!(replayed, 0, "covered task replay on Work join at {fresh}");
        controller.set_work_watch(None).unwrap();
        work_next_watch(&mut peer).await;
        peer.fence(&rig.commands).await;
    }
    controller.shutdown();
    rig.stop().await;
}

// Break caught: a read whose org.tree predates the join activates its task topics.
#[tokio::test]
async fn work_join_ignores_pre_join_snapshot_completion() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    controller.refresh();
    peer.reply("org.tree", tree()).await;
    let (held, _) = peer.request("decision.list").await;
    controller
        .set_work_watch(Some(work_interest(1065)))
        .unwrap();
    peer.fence(&rig.commands).await;
    assert_eq!(
        peer.watches.len(),
        1,
        "pre-join read must leave the old watch live"
    );
    peer.send(DaemonMessage::response_ok(held, page(vec![], None)))
        .await;
    apply_next(&mut controller).await;
    work_finish_join(&mut controller, &mut peer, 1065).await;
    assert_eq!(peer.watches.last().unwrap().1, 1065);
    controller.shutdown();
    rig.stop().await;
}

// Break caught: taking max loses the older consumer's tail, or repeated joins spawn extra reads.
#[tokio::test]
async fn work_join_keeps_minimum_of_fresh_floors() {
    for (decision, work) in [(1065, 1070), (1070, 1065)] {
        let (mut rig, mut peer) = Rig::start(METHODS).await;
        let mut controller = rig.controller();
        work_snapshot(&mut controller, &mut peer, 41).await;
        peer.fence(&rig.commands).await;
        controller
            .set_work_watch(Some(work_interest(work)))
            .unwrap();
        work_finish_join(&mut controller, &mut peer, decision).await;
        assert_eq!(peer.watches.last().unwrap().1, 1065);
        for (seq, kind) in [(1066, "task.usage_observed"), (1067, "decision.requested")] {
            peer.send(DaemonMessage::Event {
                topic: "org".into(),
                seq,
                ts: "2026-01-15T12:00:00Z".into(),
                kind: kind.into(),
                body: json!({}),
            })
            .await;
        }
        let mut seen = Vec::new();
        while seen.len() < 2 {
            if let Event::Control(atm_tui::control::ControlNotice::Event(DaemonMessage::Event {
                seq,
                ..
            })) = guarded(rig._events.recv()).await.unwrap()
            {
                seen.push(seq);
            }
        }
        assert_eq!(seen, [1066, 1067]);
        controller.shutdown();
        rig.stop().await;
    }
}

// Break caught: repeated/advanced interest installs an intermediate stale union or parallel read.
#[tokio::test]
async fn work_join_repeated_interest_coalesces_refresh() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    controller
        .set_work_watch(Some(work_interest(1065)))
        .unwrap();
    let held = work_join_org(&mut peer).await;
    controller
        .set_work_watch(Some(work_interest(1065)))
        .unwrap();
    controller
        .set_work_watch(Some(work_interest(1070)))
        .unwrap();
    peer.fence(&rig.commands).await;
    assert_eq!(peer.watches.len(), 1);
    let mut org = tree();
    org["seq"] = json!(1070);
    peer.send(DaemonMessage::response_ok(held, org)).await;
    peer.reply("decision.list", page(vec![], None)).await;
    apply_next(&mut controller).await;
    work_next_watch(&mut peer).await;
    peer.fence(&rig.commands).await;
    assert_eq!(peer.watches.len(), 2);
    assert_eq!(peer.watches.last().unwrap().1, 1070);
    controller.shutdown();
    rig.stop().await;
}

// Break caught: a late join read restores hidden Work, or reopening accepts that old read.
#[tokio::test]
async fn work_join_hidden_and_reopened_does_not_activate_obsolete_interest() {
    for reopen_while_held in [false, true] {
        let (mut rig, mut peer) = Rig::start(METHODS).await;
        let mut controller = rig.controller();
        work_snapshot(&mut controller, &mut peer, 41).await;
        peer.fence(&rig.commands).await;
        controller
            .set_work_watch(Some(work_interest(1065)))
            .unwrap();
        let held = work_join_org(&mut peer).await;
        controller.set_work_watch(None).unwrap();
        work_wire_fence(&mut rig, &mut peer).await;
        controller.set_work_watch(None).unwrap();
        peer.fence(&rig.commands).await;
        assert_eq!(peer.watches.len(), 2);
        if reopen_while_held {
            controller
                .set_work_watch(Some(work_interest(2089)))
                .unwrap();
        }
        let mut org = tree();
        org["seq"] = json!(1065);
        peer.send(DaemonMessage::response_ok(held, org)).await;
        peer.reply("decision.list", page(vec![], None)).await;
        apply_next(&mut controller).await;
        if !reopen_while_held {
            work_next_watch(&mut peer).await;
            assert_eq!(
                peer.watches.last().unwrap().0,
                ["decision.", "agent.", "project."]
            );
            controller
                .set_work_watch(Some(work_interest(2089)))
                .unwrap();
        }
        work_finish_join(&mut controller, &mut peer, 2089).await;
        assert_eq!(peer.watches.last().unwrap().1, 2089);
        peer.fence(&rig.commands).await;
        controller.shutdown();
        rig.stop().await;
    }
}

// Break caught: a refused/malformed join refresh strands Work or fabricates a newer floor.
#[tokio::test]
async fn work_join_refresh_failure_preserves_safe_watch() {
    for malformed in [false, true] {
        let (mut rig, mut peer) = Rig::start(METHODS).await;
        let mut controller = rig.controller();
        work_snapshot(&mut controller, &mut peer, 41).await;
        peer.fence(&rig.commands).await;
        controller
            .set_work_watch(Some(work_interest(1065)))
            .unwrap();
        let held = work_join_org(&mut peer).await;
        peer.send(if malformed {
            DaemonMessage::response_ok(held, json!({"seq":"invalid"}))
        } else {
            DaemonMessage::response_err(held, "unavailable", "held projection refused")
        })
        .await;
        apply_next(&mut controller).await;
        peer.fence(&rig.commands).await;
        work_wire_fence(&mut rig, &mut peer).await;
        assert_eq!(
            peer.watches.len(),
            2,
            "failed join must activate one safe union"
        );
        assert_eq!(peer.watches.last().unwrap().1, 41);
        assert!(peer
            .watches
            .last()
            .unwrap()
            .0
            .iter()
            .any(|topic| topic == "task."));
        assert!(controller.waiting_panes.is_empty());
        peer.fence(&rig.commands).await;
        assert_eq!(
            peer.watches.len(),
            2,
            "one conservative fallback, no retry loop"
        );
        peer.send(DaemonMessage::Event {
            topic: "task".into(),
            seq: 1066,
            ts: "2026-01-15T12:00:00Z".into(),
            kind: "task.usage_observed".into(),
            body: json!({}),
        })
        .await;
        loop {
            if let Event::Control(
                notice @ atm_tui::control::ControlNotice::Event(DaemonMessage::Event {
                    seq: 1066,
                    ..
                }),
            ) = guarded(rig._events.recv()).await.unwrap()
            {
                controller.notice(&notice);
                break;
            }
        }
        peer.fence(&rig.commands).await;
        controller.shutdown();
        rig.stop().await;
    }
}

// Break caught: a timed-out fresh read leaves the join gate permanently armed.
#[tokio::test(start_paused = true)]
async fn work_join_refresh_timeout_falls_back_without_real_socket_clock() {
    let (tx, mut commands) = mpsc::unbounded_channel();
    let mut controller = DecisionController::new(ControlHandle::new(tx), None);
    controller.refresh();
    let old = guarded(commands.recv()).await.unwrap();
    controller
        .set_work_watch(Some(work_interest(1065)))
        .unwrap();
    tokio::time::advance(Duration::from_secs(17)).await;
    apply_next(&mut controller).await;
    let fresh = guarded(commands.recv()).await.unwrap();
    assert!(format!("{fresh:?}").contains("org.tree"));
    assert!(commands.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(17)).await;
    apply_next(&mut controller).await;
    let fallback = commands
        .try_recv()
        .expect("fresh timeout must synchronously submit the fallback");
    let fallback = format!("{fallback:?}");
    assert!(fallback.contains("Watch { topics:"), "{fallback}");
    assert!(
        fallback.contains("task.") && fallback.contains("cursor: 1065"),
        "{fallback}"
    );
    assert!(commands.try_recv().is_err());
    controller.shutdown();
    drop((old, fresh));
}

// Break caught: a disconnected join gate blocks decision-only recovery when Work does not rejoin.
#[tokio::test]
async fn work_join_disconnected_without_rejoin_recovers_decision_only() {
    use atm_tui::control::ControlNotice;
    let (mut rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    controller
        .set_work_watch(Some(work_interest(1065)))
        .unwrap();
    let old = work_join_org(&mut peer).await;
    controller.disconnected();
    controller.set_work_watch(None).unwrap();
    assert!(matches!(peer.raw().await, MessageType::Unwatch));
    controller.notice(&ControlNotice::Capabilities {
        protocol_version: ProtocolVersion::new(1, 1),
        methods: METHODS.iter().map(|m| (*m).into()).collect(),
    });
    let fresh = work_join_org(&mut peer).await;
    peer.send(DaemonMessage::response_ok(old, tree())).await;
    let mut org = tree();
    org["seq"] = json!(2089);
    peer.send(DaemonMessage::response_ok(fresh, org)).await;
    peer.reply("decision.list", page(vec![], None)).await;
    apply_next(&mut controller).await;
    peer.fence(&rig.commands).await;
    work_wire_fence(&mut rig, &mut peer).await;
    assert_eq!(
        peer.watches.len(),
        2,
        "decision watch remained blocked by disconnected join"
    );
    assert_eq!(
        peer.watches.last().unwrap(),
        &(
            vec!["decision.".into(), "agent.".into(), "project.".into()],
            2089
        )
    );
    controller.shutdown();
    rig.stop().await;
}

// Break caught: WatchFailed leaves a pending Work interest, or failure dirties a mutation.
#[tokio::test]
async fn work_join_failure_during_open_get_or_mutation_keeps_op7_gate() {
    use atm_tui::control::{ControlFailure, ControlNotice};
    for mutating in [false, true] {
        let (rig, mut peer) = Rig::start(METHODS).await;
        let mut controller = rig.controller();
        work_snapshot(&mut controller, &mut peer, 41).await;
        let held_popup = if mutating {
            open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
            controller.key(key(KeyCode::Enter));
            peer.request("decision.resolve").await.0
        } else {
            controller.open(atm_org::DecisionId::new(41));
            peer.request("decision.get").await.0
        };
        controller
            .set_work_watch(Some(work_interest(1065)))
            .unwrap();
        let held_join = work_join_org(&mut peer).await;
        controller.notice(&ControlNotice::WatchFailed(ControlFailure::Timeout));
        controller.set_work_watch(None).unwrap();
        assert!(matches!(peer.raw().await, MessageType::Unwatch));
        peer.send(DaemonMessage::response_err(
            held_join,
            "unavailable",
            "join failed",
        ))
        .await;
        apply_next(&mut controller).await;
        peer.send(if mutating {
            DaemonMessage::response_err(held_popup, "unavailable", "reply lost after admission")
        } else {
            DaemonMessage::response_ok(held_popup, json!({"decision":entry(41,"alpha/lead")}))
        })
        .await;
        apply_popup(&mut controller).await;
        controller.key(key(KeyCode::Enter));
        if mutating {
            assert!(
                rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown")
            );
        } else {
            assert!(
                matches!(controller.popup.as_ref().unwrap().screen, Screen::Loading),
                "an old loading reply must remain gated after the pending join fails"
            );
            assert!(
                !rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Recording answer")
            );
            let (fresh, _) = peer.request("decision.get").await;
            peer.send(DaemonMessage::response_ok(
                fresh,
                json!({"decision":provisional_entry()}),
            ))
            .await;
            apply_popup(&mut controller).await;
        }
        peer.fence(&rig.commands).await;
        assert_eq!(
            peer.watches.len(),
            1,
            "failed pending join must not restore Work"
        );
        controller.shutdown();
        rig.stop().await;
    }
}

// Break caught: max(cursor), lost decision/project topics, or reordered legacy topics.
#[test]
fn work_watch_union_uses_earliest_snapshot_cursor() {
    for (decision, work, expected) in [(41, 30, 30), (41, 70, 41)] {
        let scope = watch_scope(Some(decision), Some(&work_interest(work))).unwrap();
        assert_eq!(scope.1, expected);
        assert_eq!(
            scope.0,
            [
                "decision.",
                "agent.",
                "project.",
                "task.",
                "msg.delivery_failed"
            ]
        );
    }
    assert_eq!(
        watch_scope(Some(41), None).unwrap(),
        (
            vec!["decision.".into(), "agent.".into(), "project.".into()],
            41
        )
    );
    assert_eq!(
        watch_scope(None, Some(&work_interest(30))).unwrap(),
        (
            vec![
                "task.".into(),
                "decision.".into(),
                "agent.".into(),
                "msg.delivery_failed".into()
            ],
            30
        )
    );
    assert_eq!(watch_scope(None, None), None);
}

// Break caught: hiding Work clears VW4's sole Watch or a repeated release re-watches.
#[tokio::test]
async fn work_hidden_releases_only_its_interest() {
    let (mut rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    peer.fence(&rig.commands).await;
    assert_eq!(
        peer.watches,
        vec![(
            vec![
                "task.".into(),
                "decision.".into(),
                "agent.".into(),
                "msg.delivery_failed".into()
            ],
            30
        )]
    );
    work_snapshot(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    assert_eq!(
        peer.watches.last().unwrap(),
        &(
            vec![
                "decision.".into(),
                "agent.".into(),
                "project.".into(),
                "task.".into(),
                "msg.delivery_failed".into()
            ],
            30
        )
    );
    controller.set_work_watch(None).unwrap();
    peer.watch_at(41).await;
    peer.fence(&rig.commands).await;
    assert_eq!(
        peer.watches.last().unwrap(),
        &(
            vec!["decision.".into(), "agent.".into(), "project.".into()],
            41
        )
    );
    controller.set_work_watch(None).unwrap();
    peer.fence(&rig.commands).await;
    assert_eq!(peer.watches.len(), 3);
    let notice = work_event(&mut rig, &mut peer, 42).await;
    controller.notice(&notice);
    let mut org = tree();
    org["seq"] = json!(42);
    peer.reply("org.tree", org).await;
    peer.reply("decision.list", page(vec![], None)).await;
    apply_next(&mut controller).await;
    peer.fence(&rig.commands).await;
    assert!(controller.waiting_panes.is_empty());
    peer.watch_at(42).await;
    assert_eq!(peer.watches.last().unwrap().1, 42);
    controller.shutdown();
    rig.stop().await;
}

// Break caught: every decision refresh rewinds Work to its first snapshot.
#[tokio::test]
async fn work_union_refresh_uses_advanced_work_floor() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    work_finish_join(&mut controller, &mut peer, 41).await;
    peer.watch_at(30).await;
    peer.fence(&rig.commands).await;
    controller.notice(&atm_tui::control::ControlNotice::Event(
        DaemonMessage::Event {
            topic: "org".into(),
            seq: 35,
            ts: "2026-01-15T12:00:00Z".into(),
            kind: "task.claimed".into(),
            body: json!({}),
        },
    ));
    controller.advance_work_cursor(60);
    peer.fence(&rig.commands).await;
    assert_eq!(
        peer.watches.len(),
        2,
        "floor updates must not issue commands"
    );
    work_snapshot(&mut controller, &mut peer, 70).await;
    peer.fence(&rig.commands).await;
    assert_eq!(peer.watches.last().unwrap().1, 60);
    controller.disconnected();
    controller.set_work_watch(None).unwrap();
    peer.fence(&rig.commands).await;
    let n = peer.watches.len();
    controller.notice(&atm_tui::control::ControlNotice::Capabilities {
        protocol_version: ProtocolVersion::new(1, 1),
        methods: METHODS.iter().map(|s| (*s).into()).collect(),
    });
    let mut org = tree();
    org["seq"] = json!(80);
    peer.reply("org.tree", org).await;
    peer.reply("decision.list", page(vec![], None)).await;
    apply_next(&mut controller).await;
    peer.fence(&rig.commands).await;
    peer.watch_at(80).await;
    assert_eq!(peer.watches.len(), n + 1);
    assert_eq!(
        peer.watches.last().unwrap(),
        &(
            vec!["decision.".into(), "agent.".into(), "project.".into()],
            80
        )
    );
    controller.shutdown();
    rig.stop().await;
}

// Lead condition (b): Work's lower cursor replays a covered ID, not a new invalidation.
#[tokio::test]
async fn work_union_replay_does_not_dirty_a_covered_decision() {
    let (mut rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    work_finish_join(&mut controller, &mut peer, 41).await;
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    peer.fence(&rig.commands).await;
    for seq in [40, 41] {
        controller.notice(&work_event(&mut rig, &mut peer, seq).await);
        peer.fence(&rig.commands).await; // any extra get is a failure, not a timing assertion
    }
    controller.key(key(KeyCode::Enter));
    let (_, params) = peer.request("decision.resolve").await;
    assert_eq!(params["id"], "d41");
    controller.shutdown();
    rig.stop().await;
}

// Lead condition (a): union delivery above the decision snapshot still gates the draft.
#[tokio::test]
async fn work_union_newer_event_invalidates_open_popup() {
    let (mut rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    work_finish_join(&mut controller, &mut peer, 41).await;
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    peer.fence(&rig.commands).await;
    controller.notice(&work_event(&mut rig, &mut peer, 42).await);
    controller.key(key(KeyCode::Enter));
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40)
        .contains("refreshing before answering"));
    let mut get = None;
    let mut org = None;
    while get.is_none() || org.is_none() {
        let MessageType::Request { id, method, .. } = peer.recv().await else {
            panic!("expected post-event reads")
        };
        match method.as_str() {
            "decision.get" => {
                assert!(get.replace(id).is_none());
            }
            "org.tree" => {
                assert!(org.replace(id).is_none());
            }
            _ => panic!("stale submission: {method}"),
        }
    }
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_ok(
        get.unwrap(),
        json!({"decision":provisional_entry()}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Char('j'))); // a deliberate different option
    controller.key(key(KeyCode::Enter));
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("[y/N]"));
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

// Break caught: a newer union event during get is overwritten by its older completion.
#[tokio::test]
async fn work_union_newer_event_during_get_requires_follow_up() {
    let (mut rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    work_finish_join(&mut controller, &mut peer, 41).await;
    peer.fence(&rig.commands).await;
    controller.open(atm_org::DecisionId::new(41));
    let (old, _) = peer.request("decision.get").await;
    controller.notice(&work_event(&mut rig, &mut peer, 42).await);
    let _held_org = peer.request("org.tree").await;
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_ok(
        old,
        json!({"decision":entry(41,"alpha/lead")}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Enter));
    assert!(!rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Recording answer"));
    let (fresh, _) = peer.request("decision.get").await;
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_ok(
        fresh,
        json!({"decision":provisional_entry()}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Enter));
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("[y/N]"));
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

// Break caught: Work release adds another physical unwatch or weakens OP7's failure gate.
#[tokio::test]
async fn work_watch_failure_preserves_open_draft_gate() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    work_finish_join(&mut controller, &mut peer, 41).await;
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    peer.watch_at(30).await;
    peer.fence(&rig.commands).await;
    controller.notice(&atm_tui::control::ControlNotice::WatchFailed(
        atm_tui::control::ControlFailure::Timeout,
    ));
    controller.set_work_watch(None).unwrap();
    controller.key(key(KeyCode::Enter));
    assert!(controller.waiting_panes.is_empty());
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40)
        .contains("refreshing before answering"));
    let mut unwatch = 0;
    let held = loop {
        match peer.raw().await {
            MessageType::Unwatch => unwatch += 1,
            MessageType::Request { id, method, .. } if method == "decision.get" => break id,
            other => panic!("unexpected failure command: {other:?}"),
        }
    };
    assert_eq!(unwatch, 1);
    peer.fence(&rig.commands).await;
    peer.send(DaemonMessage::response_ok(
        held,
        json!({"decision":provisional_entry()}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Enter));
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("[y/N]"));
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

// Break caught: an old loading reply undoes a failure observed through the union.
#[tokio::test]
async fn work_watch_failure_during_loading_requires_a_fresh_get() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    work_finish_join(&mut controller, &mut peer, 41).await;
    peer.watch_at(30).await;
    controller.open(atm_org::DecisionId::new(41));
    let (held, _) = peer.request("decision.get").await;
    controller.notice(&atm_tui::control::ControlNotice::WatchFailed(
        atm_tui::control::ControlFailure::Timeout,
    ));
    controller.set_work_watch(None).unwrap();
    assert!(matches!(peer.raw().await, MessageType::Unwatch));
    peer.send(DaemonMessage::response_ok(
        held,
        json!({"decision":entry(41,"alpha/lead")}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Enter));
    assert!(!rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Recording answer"));
    let MessageType::Request {
        id: fresh, method, ..
    } = peer.raw().await
    else {
        panic!("expected the one post-failure get")
    };
    assert_eq!(method, "decision.get");
    peer.fence(&rig.commands).await;
    assert_eq!(peer.watches.len(), 2);
    peer.send(DaemonMessage::response_ok(
        fresh,
        json!({"decision":provisional_entry()}),
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Char('j')));
    controller.key(key(KeyCode::Enter));
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("[y/N]"));
    peer.fence(&rig.commands).await;
    controller.shutdown();
    rig.stop().await;
}

// Break caught: releasing Work after failure replaces a mutation or replays its unknown outcome.
#[tokio::test]
async fn work_watch_failure_during_mutation_never_replays_the_answer() {
    let (rig, mut peer) = Rig::start(METHODS).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut controller = rig.controller();
    work_snapshot(&mut controller, &mut peer, 41).await;
    controller.set_work_watch(Some(work_interest(30))).unwrap();
    work_finish_join(&mut controller, &mut peer, 41).await;
    open_peer_entry(&mut controller, &mut peer, entry(41, "alpha/lead")).await;
    peer.watch_at(30).await;
    controller.key(key(KeyCode::Enter));
    let (held, _) = peer.request("decision.resolve").await;
    let failed =
        atm_tui::control::ControlNotice::WatchFailed(atm_tui::control::ControlFailure::Timeout);
    controller.notice(&failed);
    controller.set_work_watch(None).unwrap();
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.raw().await, MessageType::Unwatch));
    assert!(matches!(peer.raw().await, MessageType::Discover));
    peer.send(DaemonMessage::response_err(
        held,
        "unavailable",
        "reply lost after admission",
    ))
    .await;
    apply_popup(&mut controller).await;
    controller.notice(&failed);
    controller.key(key(KeyCode::Enter));
    peer.fence(&rig.commands).await;
    assert_eq!(peer.watches.len(), 2);
    assert!(rendered(controller.popup.as_mut().unwrap(), 100, 40).contains("Outcome unknown"));
    controller.shutdown();
    rig.stop().await;
}

#[tokio::test]
async fn unavailable_and_malformed_pages_are_not_an_empty_queue() {
    let (rig, mut peer) = Rig::start(&["org.tree"]).await;
    let mut controller = rig.controller();
    controller.open_for(None);
    peer.reply("org.tree", tree()).await;
    apply_next(&mut controller).await;
    assert!(rendered(controller.popup.as_mut().unwrap(), 80, 12)
        .contains("does not support decision.list"));
    controller.shutdown();
    rig.stop().await;
    let (rig, mut peer) = Rig::start(METHODS).await;
    let mut controller = rig.controller();
    controller.open_for(None);
    peer.reply("org.tree", tree()).await;
    peer.reply("decision.list", page(vec![entry(2, "system")], Some(1)))
        .await;
    apply_next(&mut controller).await;
    assert!(matches!(
        controller.popup.as_ref().unwrap().screen,
        Screen::Error(_)
    ));
    controller.shutdown();
    rig.stop().await;
}
