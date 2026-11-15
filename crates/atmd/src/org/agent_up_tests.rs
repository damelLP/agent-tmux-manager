// `agent.up` over the real control actor and a scripted tmux client.
//
// Included after `agent_clear_tests.rs`, into the same module: no module docs,
// and the helpers of the two files before it are in scope. A start reads its
// role from the filesystem first, which a paused clock would time out, so each
// test runs on the real clock until the flow has read it and pauses the clock
// then: from there all I/O is in memory. Tests synchronise on committed
// events, the flow's own progress events and the order of scripted reads.
// Every "nothing was sent" test names its refusal and has a twin that sends.
//
// The harness screens are the adapters' real captures. The shell screens are
// synthetic: a prompt `pb> ` and what was typed after it.

use crate::org::agent_launch::launch_command;
use crate::org::agent_project::PreparedAgent;

const CLAUDE_READY: &str = fixture!("claude", "control_p1_A120-ready-120x40_2.1.325");
/// Codex's update offer at its start (a plain capture, read as a styled one
/// without styles).
const CODEX_UPDATE: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_codex-startup_0.167.0.txt"
);
const PROMPT: &str = "pb> ";
const WIDTH: usize = 120;
const START: &str = "start did not complete";
const NOT_RUN: &str = "the launch command was typed and not run";

thread_local! {
    /// Whether this test (each runs on its own thread) has paused its clock.
    static PAUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn started() -> AgentId {
    "q/lead".parse().unwrap()
}

fn idle_shell() -> ProcFacts {
    ProcFacts {
        process_group: 100,
        foreground_group: 100,
        system_shell: true,
        children: 0,
        name: "zsh".into(),
    }
}

/// (harness, its ready editor, the command tmux shows for it)
const HARNESSES: [(Harness, &str, &str); 3] = [
    (Harness::ClaudeCode, CLAUDE_READY, "claude"),
    (Harness::Pi, PI_PADDED, "pi"),
    (Harness::Codex, CODEX_FRESH, "codex"),
];

/// Role `lead` of project `q` on disk, the service started, the caller
/// outside tmux: the pane to split is `%0` and the new pane will be `%1`.
struct Starting {
    rig: Rig,
    agents: AgentHandle,
    events: broadcast::Receiver<crate::org::agents::ObservationProgress>,
    project: tempfile::TempDir,
    /// The launch text the flow types.
    text: String,
}

impl Starting {
    async fn new(harness: Harness) -> Self {
        Self::with_role(Rig::new().await, harness, "").await
    }

    /// `more` is appended to the role's front matter.
    async fn with_role(rig: Rig, harness: Harness, more: &str) -> Self {
        Self::on(rig, tempfile::tempdir().unwrap(), harness, more).await
    }

    /// As [`Self::with_role`], with the project in `project`'s directory `q`.
    async fn on(rig: Rig, project: tempfile::TempDir, harness: Harness, more: &str) -> Self {
        let root = project.path().join("q");
        std::fs::create_dir_all(root.join(".atm/roles")).unwrap();
        let role = format!("+++\nharness='{}'\n{more}+++\n", harness.short_tag());
        std::fs::write(root.join(".atm/roles/lead.md"), &role).unwrap();
        let root = root.canonicalize().unwrap();
        let text = launch_command(
            &PreparedAgent {
                agent_id: started(),
                root: root.clone(),
                role: atm_org::parse_role("lead", &role).unwrap(),
                cwd: root,
                trust_dirs: Vec::new(),
            },
            |_| None,
        )
        .map(|command| command.text)
        .unwrap_or_default();
        *rig.proc.lock().unwrap() = Some(idle_shell());
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let events = agents.observation_events();
        Self {
            rig,
            agents,
            events,
            project,
            text,
        }
    }

    fn inspection(pane: &str, tag: Option<&str>, command: &str) -> PaneInspection {
        let mut inspection = Rig::inspection(pane, "", false)
            .with_foreground_process(None, Some(command.into()));
        inspection.local_agent_tag = tag.map(Into::into);
        inspection.pane.is_active = false;
        inspection
    }

    /// The new pane's shell with `line` on its last line and the cursor
    /// after it, wrapped as a terminal 120 cells wide wraps it; nobody focused.
    fn shell(line: &str) -> PaneCapture {
        let cells: Vec<char> = line.chars().collect();
        let mut rows: Vec<String> = cells.chunks(WIDTH).map(|row| row.iter().collect()).collect();
        if cells.len().is_multiple_of(WIDTH) {
            rows.push(String::new());
        }
        let cursor = (cells.len() % WIDTH, rows.len() - 1);
        rows.resize(10, String::new());
        let mut inspection = Self::inspection("%1", Some("q/lead"), "zsh");
        inspection.pane.height = rows.len() as u16;
        inspection.cursor_column = cursor.0 as u32;
        inspection.cursor_row = cursor.1 as u32;
        PaneCapture::new(rows.join("\n") + "\n", inspection, CaptureFormat::Ansi)
            .with_focus(Some(false))
    }

    /// The new pane showing a real capture `text` with `command` in front.
    fn screen(text: &str, command: &str) -> PaneCapture {
        let text = body(text);
        let mut inspection = Self::inspection("%1", Some("q/lead"), command);
        inspection.pane.height = text.lines().count() as u16;
        PaneCapture::new(text.into(), inspection, CaptureFormat::Ansi).with_focus(Some(false))
    }

    /// Script the next capture or guard of the new pane.
    fn frame(&self, capture: PaneCapture) {
        self.rig.client.script_capture(&capture).unwrap();
    }

    fn staged(&self) -> PaneCapture {
        Self::shell(&format!("{PROMPT}{}", self.text))
    }

    /// The reads up to the new pane's tag: the pane to split, then the tag.
    fn script_pane(&self) {
        self.rig
            .client
            .script_inspect(&pane("%0"), Some(&Self::inspection("%0", None, "zsh")))
            .unwrap();
        self.rig.client.mock().set_next_pane_id("%1");
        self.rig
            .client
            .script_tag_check(&Self::inspection("%1", None, "zsh"))
            .unwrap();
    }

    /// The frames from the idle shell to the paste: two captures that prove
    /// the shell idle, and the paste's guard.
    fn script_paste(&self) {
        for _ in 0..3 {
            self.frame(Self::shell(PROMPT));
        }
    }

    /// Everything a start reads up to and including its Enter.
    fn script_launch(&self) {
        self.script_pane();
        self.script_paste();
        self.frame(self.staged());
        self.frame(self.staged());
    }

    /// Script the inspection of one observation of the agent after its start.
    fn follow_up(&self) {
        self.rig
            .client
            .script_inspect(
                &pane("%1"),
                Some(&Self::inspection("%1", Some("q/lead"), "zsh")),
            )
            .unwrap();
    }

    fn params(&self) -> UpParams {
        UpParams {
            project_root: self.project.path().join("q"),
            name: "lead".into(),
            caller_server: None,
        }
    }

    /// Start the agent, and pause the clock once the flow has read its role.
    async fn start(&mut self) -> tokio::task::JoinHandle<Result<UpResult, AgentError>> {
        let (agents, params) = (self.agents.clone(), self.params());
        // An earlier start of this test paused the clock.
        if PAUSED.replace(false) {
            tokio::time::resume();
        }
        let call = tokio::spawn(async move { agents.up(params, caller()).await });
        progress(&mut self.events, "up_prepared", Some("q/lead")).await;
        tokio::time::pause();
        PAUSED.set(true);
        call
    }

    async fn up(&mut self) -> Result<UpResult, AgentError> {
        self.start().await.await.unwrap()
    }

    /// Every observation the events so far started has ended.
    async fn settled(&mut self) {
        self.events = self.agents.observation_events();
        self.rig
            .events
            .send(crate::registry::SessionEvent::Removed {
                session_id: SessionId::new("unrelated"),
                reason: crate::registry::RemovalReason::Explicit,
            })
            .unwrap();
        progress(&mut self.events, "idle", None).await;
    }

    /// What was done to panes, in order.
    fn effects(&self) -> Vec<String> {
        self.rig
            .client
            .mock()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                MockCall::SplitWindow {
                    target,
                    direction,
                    detached,
                    command,
                    ..
                } => Some(format!(
                    "split {target} {direction:?} detached={detached} command={command:?}"
                )),
                MockCall::RawCommand { subcommand, args } if subcommand == "set-option" => {
                    Some(format!("set-option {}", args.join(" ")))
                }
                MockCall::PasteText { pane, text } => Some(format!("paste {pane} {text}")),
                MockCall::SendKeys { pane, keys } => Some(format!("keys {pane} {keys}")),
                MockCall::KillPane { pane } => Some(format!("kill {pane}")),
                _ => None,
            })
            .collect()
    }

    /// The effects of a launch through its Enter.
    fn launched(&self) -> Vec<String> {
        vec![
            "split %0 Below detached=true command=None".into(),
            "set-option -p -t %1 @atm-agent q/lead".into(),
            format!("paste %1 {}", self.text),
            "keys %1 Enter".into(),
        ]
    }

    fn unread(&self) -> usize {
        self.rig.client.pending_reads(&pane("%1")).unwrap()
            + self.rig.client.pending_reads(&pane("%0")).unwrap_or(0)
    }

    async fn agent(&self) -> Option<atm_org::Agent> {
        let state = self.rig.org.snapshot().await.unwrap();
        state.agent(&started()).cloned()
    }

    /// The agent is Stuck for `reason`, on `%1`, and its owner was told once.
    async fn assert_stuck(&self, reason: &str) {
        let agent = self.agent().await.unwrap();
        assert_eq!(agent.status, AgentStatus::Stuck, "{agent:?}");
        let expected = format!("{START}: {reason}; inspect %1");
        assert_eq!(agent.reason.as_deref(), Some(expected.as_str()));
        let state = self.rig.org.snapshot().await.unwrap();
        let notes = state
            .messages()
            .filter(|message| {
                message.content.to == atm_org::Address::Owner
                    && message.content.body == format!("agent q/lead: {expected}")
            })
            .count();
        assert_eq!(notes, 1);
    }
}

#[tokio::test]
async fn up_order_is_split_tag_pane_created_paste_enter_ready() {
    // Also shell_right_after_launch_enter_waits_then_harness_pins (the first
    // capture after the Enter still shows the shell in front) and
    // codex_up_ready_is_fresh_editor_only (no session is waited for).
    for (harness, ready, command) in HARNESSES {
        let mut starting = Starting::new(harness).await;
        let mut committed = starting.rig.org.committed.subscribe();
        starting.script_launch();
        // The command runs: the shell is still in front, then the harness.
        starting.frame(starting.staged());
        starting.frame(Starting::screen(ready, command));
        let announces = harness != Harness::Codex;
        if announces {
            // After the session: the editor again.
            starting.frame(Starting::screen(ready, command));
        }
        // The start's commits, and the session's events, are observed once
        // the start has released the agent.
        starting.follow_up();
        if !announces {
            starting.follow_up();
        }
        let call = starting.start().await;
        if announces {
            progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
            // Launched, the harness's editor is up, and no session yet.
            assert_eq!(starting.effects(), starting.launched(), "{harness:?}");
            let agent = starting.agent().await.unwrap();
            assert_eq!(agent.status, AgentStatus::Starting);
            assert_eq!(agent.pane.as_deref(), Some("%1"));
            starting.follow_up();
            start_session(&starting.rig, harness, "s1", Some("startup")).await;
        }
        let result = call.await.unwrap().unwrap();
        assert_eq!(result.agent_id, started());
        assert_eq!(result.pane, "%1");
        assert_eq!(result.state, AgentStatus::Ready);
        assert_eq!(starting.effects(), starting.launched(), "{harness:?}");
        let agent = starting.agent().await.unwrap();
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(
            agent.session_id,
            announces.then(|| SessionId::new("s1")),
            "{harness:?}"
        );
        let mut log = Vec::new();
        while let Ok(event) = committed.try_recv() {
            log.push(event);
        }
        assert_eq!(
            log.iter().map(|event| event.kind.as_str()).collect::<Vec<_>>(),
            [
                "project.registered",
                "agent.up_requested",
                "agent.pane_created",
                "control.spawned",
                if announces {
                    "agent.session_bound"
                } else {
                    "agent.editor_ready"
                },
            ],
            "{harness:?}"
        );
        // The launch is audited with the binary it ran.
        assert_eq!(
            log.get(3).map(|event| &event.body),
            Some(&serde_json::json!({
                "pane": "%1",
                "action": { "method": "agent.up", "agent": "q/lead", "binary": command },
            })),
            "{harness:?}"
        );
        // The pane's shell and the harness's name are recorded.
        assert_eq!(
            starting.agents.baseline_of(&started(), "%1"),
            PaneBaseline::shell(100, "zsh".into())
                .with_harness(command.into())
                .unwrap()
        );
        starting.settled().await;
        assert_eq!(starting.unread(), 0, "{harness:?}");
    }
}

#[tokio::test]
async fn up_ready_needs_session_then_safe_editor() {
    // A harness that announces its session is ready on that session and then
    // a safe editor. Neither alone: the editor before the session waits (the
    // test above), and the session before a safe editor waits here.
    for (harness, ready, busy, command) in [
        (Harness::ClaudeCode, CLAUDE_READY, CLAUDE_RUNNING, "claude"),
        (Harness::Pi, PI_PADDED, PI_RUNNING, "pi"),
    ] {
        let mut starting = Starting::new(harness).await;
        starting.script_launch();
        starting.frame(Starting::screen(ready, command));
        // With the session: a turn is running, then the editor is safe.
        starting.frame(Starting::screen(busy, command));
        starting.frame(Starting::screen(ready, command));
        starting.follow_up();
        let call = starting.start().await;
        progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
        let mut committed = starting.rig.org.committed.subscribe();
        starting.follow_up();
        start_session(&starting.rig, harness, "s1", Some("startup")).await;
        let result = call.await.unwrap().unwrap();
        assert_eq!(result.state, AgentStatus::Ready);
        assert_eq!(committed_kinds(&mut committed), ["agent.session_bound"]);
        starting.settled().await;
        assert_eq!(starting.unread(), 0, "{harness:?}");
    }
}

#[tokio::test]
async fn status_only_row_does_not_make_a_start_ready() {
    // A row on the new pane that no accepted start created (a status line, a
    // discovery scan) is not the harness announcing its session. Twin: the
    // start event, in up_order_is_split_tag_pane_created_paste_enter_ready.
    let mut starting = Starting::new(Harness::ClaudeCode).await;
    starting.script_launch();
    starting.frame(Starting::screen(CLAUDE_READY, "claude"));
    starting.frame(Starting::screen(CLAUDE_READY, "claude"));
    starting.frame(Starting::screen(CLAUDE_READY, "claude"));
    let call = starting.start().await;
    progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
    starting.follow_up();
    starting
        .rig
        .row_of("s0", "%1", false, Harness::ClaudeCode)
        .await;
    progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
    assert_eq!(
        starting.agent().await.unwrap().status,
        AgentStatus::Starting
    );
    // A second session on the pane: nothing says which is the agent's.
    start_session(&starting.rig, Harness::ClaudeCode, "s1", Some("startup")).await;
    let result = call.await.unwrap();
    assert!(
        matches!(&result, Err(AgentError::Refused(reason)) if reason.contains("more than one session")),
        "{result:?}"
    );
    starting
        .assert_stuck("more than one session appeared on the new pane")
        .await;
    assert_eq!(starting.effects(), starting.launched());
}

#[tokio::test]
async fn up_non_ascii_launch_text_refuses_before_split() {
    // Twin: every other test here, whose role is typed.
    let mut starting =
        Starting::with_role(Rig::new().await, Harness::Pi, "model='mod\u{e8}le'\n").await;
    let params = starting.params();
    let result = starting.agents.up(params, caller()).await;
    assert!(
        matches!(&result, Err(AgentError::Refused(reason)) if reason == "launch text must be printable ASCII"),
        "{result:?}"
    );
    assert!(starting.rig.client.mock().calls().is_empty());
    assert!(starting.agent().await.is_none());
    starting.settled().await;
}

#[tokio::test]
async fn up_refuses_a_placement_it_cannot_prove() {
    // The role names an agent to be placed beside. Refused with nothing
    // changed: a hint that can't be read, an agent without a pane, and a pane
    // that no longer carries that agent's tag. Twin: the tagged pane, split.
    for (hint, tag, refusal) in [
        ("under:lead", None, "its pane hint is not"),
        ("below:nobody", None, "has no pane"),
        ("below:boss", Some("q/other"), "is no longer agent q/boss's pane"),
        ("right:boss", Some("q/boss"), ""),
    ] {
        // `q/boss` is recorded Ready on %7 before the service starts; its
        // restart check inspects the pane once.
        let rig = Rig::new().await;
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("q")).unwrap();
        let root = project.path().join("q").canonicalize().unwrap();
        let boss: AgentId = "q/boss".parse().unwrap();
        for command in [
            OrgCommand::RegisterProject {
                project_id: "q".parse().unwrap(),
                root: root.to_str().unwrap().into(),
                move_root: false,
            },
            OrgCommand::Up {
                agent_id: boss.clone(),
                role: atm_org::parse_role("boss", "+++\nharness='codex'\n+++\n").unwrap(),
            },
            OrgCommand::PaneCreated {
                agent_id: boss.clone(),
                pane: "%7".into(),
            },
            OrgCommand::EditorReady {
                agent_id: boss.clone(),
            },
        ] {
            rig.command(command).await;
        }
        rig.inspect_only("%7", "q/boss");
        let mut starting =
            Starting::on(rig, project, Harness::Codex, &format!("pane='{hint}'\n")).await;
        assert_eq!(starting.rig.client.pending_reads(&pane("%7")).unwrap(), 0);
        let before = starting.rig.client.mock().calls().len();
        if let Some(tag) = tag {
            starting
                .rig
                .client
                .script_inspect(&pane("%7"), Some(&Starting::inspection("%7", Some(tag), "codex")))
                .unwrap();
        }
        if refusal.is_empty() {
            starting.rig.client.mock().set_next_pane_id("%1");
            starting
                .rig
                .client
                .script_tag_check(&Starting::inspection("%1", None, "zsh"))
                .unwrap();
            starting.script_paste();
            starting.frame(starting.staged());
            starting.frame(starting.staged());
            starting.frame(Starting::screen(CODEX_FRESH, "codex"));
            let result = starting.up().await.unwrap();
            assert_eq!(result.state, AgentStatus::Ready);
            assert_eq!(
                starting.effects().first().map(String::as_str),
                Some("split %7 Right detached=true command=None"),
            );
            continue;
        }
        let params = starting.params();
        let result = starting.agents.up(params, caller()).await;
        assert!(
            matches!(&result, Err(AgentError::Refused(reason)) if reason.contains(refusal)),
            "{hint}: {result:?}"
        );
        assert!(starting.agent().await.is_none(), "{hint}");
        let calls = starting.rig.client.mock().calls();
        assert!(
            calls.iter().skip(before).all(|call| matches!(
                call,
                MockCall::RawCommand { subcommand, .. } if subcommand != "set-option"
            ) || matches!(call, MockCall::ListPanes)),
            "{hint}: {calls:?}"
        );
        assert_eq!(starting.rig.client.pending_reads(&pane("%7")).unwrap_or(0), 0);
    }
}

/// A start that failed before its pane was recorded: marked Stuck by its
/// cleanup, and then Down by the next observation, which finds no pane.
async fn assert_down_without_pane(starting: &mut Starting, reason: &str) {
    starting.settled().await;
    let agent = starting.agent().await.unwrap();
    assert_eq!(
        (agent.status, agent.pane.clone()),
        (AgentStatus::Down, None)
    );
    assert_eq!(
        agent.reason.as_deref(),
        Some(
            format!(
                "recorded pane is absent, dead or no longer owned; previous: {START}: {reason}; inspect its pane"
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn up_failure_at_each_step_stops_later_effects() {
    // The pane to split is gone: refused, nothing recorded, nothing changed.
    let starting = Starting::new(Harness::Pi).await;
    starting
        .rig
        .client
        .script_inspect(&pane("%0"), None)
        .unwrap();
    let params = starting.params();
    let result = starting.agents.up(params, caller()).await;
    assert!(
        matches!(&result, Err(AgentError::Refused(reason)) if reason.contains("the pane to split, %0, is gone") && reason.ends_with("nothing was changed")),
        "{result:?}"
    );
    assert!(starting.agent().await.is_none());
    assert!(starting.effects().is_empty());
    // The role was read from the project, which that registered.
    assert!(starting
        .project
        .path()
        .join("q/.atm/project.toml")
        .is_file());

    // The split does not give a pane the flow can use (tmux answers with
    // something that is no pane id): the start was recorded, so its failure
    // is too, and nothing follows the split.
    let mut starting = Starting::new(Harness::Pi).await;
    starting
        .rig
        .client
        .script_inspect(&pane("%0"), Some(&Starting::inspection("%0", None, "zsh")))
        .unwrap();
    starting.rig.client.mock().set_next_pane_id("no pane");
    let result = starting.up().await;
    assert!(
        matches!(result, Err(AgentError::Reservation(_))),
        "{result:?}"
    );
    assert_eq!(
        starting.effects(),
        ["split %0 Below detached=true command=None"]
    );
    assert_down_without_pane(&mut starting, "its pane was not created").await;

    // The new pane already carries a tag: it is not tagged, recorded or typed into.
    let mut starting = Starting::new(Harness::Pi).await;
    starting
        .rig
        .client
        .script_inspect(&pane("%0"), Some(&Starting::inspection("%0", None, "zsh")))
        .unwrap();
    starting.rig.client.mock().set_next_pane_id("%1");
    starting
        .rig
        .client
        .script_tag_check(&Starting::inspection("%1", Some("q/other"), "zsh"))
        .unwrap();
    let result = starting.up().await;
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(
                crate::control::reservation::ReservationError::TagMismatch { .. }
            ))
        ),
        "{result:?}"
    );
    assert_eq!(
        starting.effects(),
        ["split %0 Below detached=true command=None"]
    );
    assert_down_without_pane(&mut starting, "%1 was created and not recorded").await;

    // The pane's record is refused by the log: nothing is typed into the pane.
    let rig = Rig::gated_with_failure(None, Some("agent.pane_created")).await;
    let mut starting = Starting::with_role(rig, Harness::Pi, "").await;
    starting.script_pane();
    let result = starting.up().await;
    assert!(
        matches!(result, Err(AgentError::Organisation(_))),
        "{result:?}"
    );
    assert_eq!(
        starting.effects(),
        [
            "split %0 Below detached=true command=None",
            "set-option -p -t %1 @atm-agent q/lead"
        ]
    );
    assert_down_without_pane(&mut starting, "%1 was created and not recorded").await;
}

#[tokio::test]
async fn up_waits_for_idle_shell_before_paste() {
    // The shell's startup file still runs a child (one capture, then the
    // process read), then the shell is idle. Nothing is pasted before that.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.rig.proc_script.lock().unwrap().extend([
        Some(shell_holding_child()),
        Some(job_in_front()),
        None,
    ]);
    starting.script_pane();
    for _ in 0..3 {
        starting.frame(Starting::shell(""));
    }
    starting.script_paste();
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert!(starting.rig.proc_script.lock().unwrap().is_empty());
    assert_eq!(starting.unread(), 0);

    // The shell is idle and has drawn no prompt yet (it is still reading its
    // startup files, with builtins only): an empty line is not a prompt, and
    // nothing is typed ahead of the prompt.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(""));
    starting.frame(Starting::shell(""));
    starting.frame(Starting::shell("   "));
    starting.frame(Starting::shell("   "));
    starting.script_paste();
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);

    // The prompt is still being drawn between the two captures of the proof:
    // that pass is not taken, the next is.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell("pb"));
    starting.frame(Starting::shell("pb>"));
    starting.script_paste();
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);

    // The twin without an idle shell: the frames run out with nothing typed.
    let mut starting = Starting::new(Harness::Codex).await;
    *starting.rig.proc.lock().unwrap() = Some(shell_holding_child());
    starting.script_pane();
    for _ in 0..3 {
        starting.frame(Starting::shell(""));
    }
    let result = starting.up().await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
    starting
        .assert_stuck("the shell in the new pane is not at its prompt")
        .await;
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn launch_refuses_focused_pane() {
    // Someone is in the new pane, or focus can't be read: nothing is typed.
    // Twin: up_order_is_split_tag_pane_created_paste_enter_ready.
    for (focused, reason) in [
        (Some(true), "the pane is focused"),
        (None, "whether the pane is focused could not be read"),
    ] {
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.frame(Starting::shell(PROMPT));
        starting.frame(Starting::shell(PROMPT).with_focus(focused));
        let result = starting.up().await;
        assert!(result.is_err(), "{result:?}");
        assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
        starting.assert_stuck(reason).await;
        assert_eq!(starting.unread(), 0);
    }
    // Focused at the paste's own guard: refused there, and looked at again.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT).with_focus(Some(true)));
    starting.script_paste();
    starting.frame(starting.staged());
    // Focused at the Enter's guard: no Enter, and looked at again.
    starting.frame(starting.staged().with_focus(Some(true)));
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn launch_line_with_typed_prefix_sends_no_enter() {
    // Also prompt_changed_between_paste_and_enter_sends_no_enter and
    // exit_to_shell_between_paste_and_enter_sends_no_key. After the paste the
    // line must read the prompt followed by the command, in the same shell.
    // Twin: up_order_is_split_tag_pane_created_paste_enter_ready.
    let mut cases: Vec<(&str, PaneCapture, String)> = Vec::new();
    {
        let starting = Starting::new(Harness::Codex).await;
        let not_staged =
            format!("{NOT_RUN}: the shell's line is not its prompt followed by the command");
        cases.push((
            "typed before the paste",
            Starting::shell(&format!("{PROMPT}rm -rf ~; {}", starting.text)),
            not_staged.clone(),
        ));
        cases.push((
            "typed after the paste",
            Starting::shell(&format!("{PROMPT}{} --yolo", starting.text)),
            not_staged.clone(),
        ));
        cases.push((
            "the prompt was drawn again",
            Starting::shell(&format!("pb (main)> {}", starting.text)),
            not_staged,
        ));
        let mut other_root = starting.staged();
        other_root.inspection.pane.pane_pid = 200;
        let mut other_command = starting.staged();
        other_command.inspection.foreground_command = Some("vim".into());
        for (label, capture) in [
            ("another root process", other_root),
            ("another program in front", other_command),
        ] {
            cases.push((
                label,
                capture,
                format!("{NOT_RUN}: the pane's shell is not known to be its foreground program"),
            ));
        }
    }
    for (label, capture, reason) in cases {
        // Seen by the flow's own capture.
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.script_paste();
        starting.frame(capture.clone());
        let result = starting.up().await;
        assert!(result.is_err(), "{label}: {result:?}");
        assert_eq!(starting.effects().len(), 3, "{label}: {:?}", starting.effects());
        starting.assert_stuck(&reason).await;
        assert_eq!(starting.unread(), 0, "{label}");

        // Staged for the flow, changed at the Enter's own guard.
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.script_paste();
        starting.frame(starting.staged());
        starting.frame(capture);
        let result = starting.up().await;
        assert!(result.is_err(), "{label}: {result:?}");
        assert_eq!(starting.effects().len(), 3, "{label}: {:?}", starting.effects());
        starting
            .assert_stuck(&format!(
                "{NOT_RUN}: the shell's line changed before the command was run"
            ))
            .await;
        assert_eq!(starting.unread(), 0, "{label}");
    }
}

#[tokio::test]
async fn refused_paste_guard_retries_with_new_prompt_line() {
    // The prompt is drawn again between the idle shell's proof and the
    // paste's guard: nothing was sent, the new line is read, and the launch
    // is typed after it.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    let redrawn = "pb (main)> ";
    for _ in 0..4 {
        starting.frame(Starting::shell(redrawn));
    }
    let staged = Starting::shell(&format!("{redrawn}{}", starting.text));
    starting.frame(staged.clone());
    starting.frame(staged);
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn draft_at_the_prompt_before_the_launch_paste_sends_nothing() {
    // The plan's draft_appears_before_guarded_paste_sends_nothing, for a launch
    // (the clear has a test of that name).
    // Something is typed at the prompt between the idle shell's proof and the
    // paste's guard. The guard refuses, and the line with the draft on it is
    // never taken as the prompt: the launch would run the draft with it.
    // Twin: refused_paste_guard_retries_with_new_prompt_line.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    let draft = format!("{PROMPT}rm -rf ~; ");
    for _ in 0..3 {
        starting.frame(Starting::shell(&draft));
    }
    let result = starting.up().await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
    starting
        .assert_stuck("the shell's prompt could not be read as settled")
        .await;
    assert_eq!(starting.unread(), 0);

    // The draft is removed again: the line reads the prompt, and the launch goes on.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    for _ in 0..3 {
        starting.frame(Starting::shell(&draft));
    }
    starting.script_paste();
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn launch_enter_outcome_unknown_never_repeats_enter() {
    // The paste, and the Enter, answer with an error that does not say
    // whether the input reached the pane: neither is sent again, and the
    // start stops. Twin: up_order_is_split_tag_pane_created_paste_enter_ready.
    for (at, what, sent) in [
        ("up_pasting", "typing the launch command", 2),
        ("up_running", "running the launch command", 3),
    ] {
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_launch();
        let call = starting.start().await;
        progress(&mut starting.events, at, Some("q/lead")).await;
        starting
            .rig
            .client
            .mock()
            .set_next_error(atm_tmux::TmuxError::CommandFailed {
                command: "probe".into(),
                stderr: "server exited unexpectedly".into(),
            });
        let result = call.await.unwrap();
        assert!(
            matches!(result, Err(AgentError::Reservation(_))),
            "{at}: {result:?}"
        );
        assert_eq!(starting.effects().len(), sent, "{at}: {:?}", starting.effects());
        let agent = starting.agent().await.unwrap();
        assert_eq!(agent.status, AgentStatus::Stuck);
        let reason = agent.reason.unwrap_or_default();
        assert!(
            reason.starts_with(&format!(
                "{START}: {what} failed, and whether it reached the pane is unknown"
            )),
            "{reason}"
        );
    }
}

#[tokio::test]
async fn a_launch_guard_refusal_that_sent_nothing_is_not_an_unknown_outcome() {
    // As for a clear: copy mode on the capture of the paste's guard, or of
    // the Enter's, sent nothing and is looked at again; a pane that carries
    // another tag by then stops the start, saying that nothing was sent.
    // Twin with an uncertain error: launch_enter_outcome_unknown_never_repeats_enter.
    let copy_mode = |mut capture: PaneCapture| {
        capture.inspection.in_mode = true;
        capture
    };
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    starting.frame(copy_mode(Starting::shell(PROMPT)));
    starting.script_paste();
    starting.frame(starting.staged());
    starting.frame(copy_mode(starting.staged()));
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);

    for (at_paste, what, sent) in [
        (true, "typing the launch command", 2),
        (false, "running the launch command", 3),
    ] {
        let other_tag = |mut capture: PaneCapture| {
            capture.inspection.local_agent_tag = Some("q/other".into());
            capture
        };
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.frame(Starting::shell(PROMPT));
        starting.frame(Starting::shell(PROMPT));
        if at_paste {
            starting.frame(other_tag(Starting::shell(PROMPT)));
        } else {
            starting.frame(Starting::shell(PROMPT));
            starting.frame(starting.staged());
            starting.frame(other_tag(starting.staged()));
        }
        let result = starting.up().await;
        assert!(
            matches!(result, Err(AgentError::Reservation(_))),
            "{result:?}"
        );
        assert_eq!(starting.effects().len(), sent, "{:?}", starting.effects());
        let reason = starting.agent().await.unwrap().reason.unwrap_or_default();
        assert!(
            reason.starts_with(&format!(
                "{START}: {what} was refused, and nothing was sent by it (pane %1 tag mismatch"
            )),
            "{reason}"
        );
        assert_eq!(starting.unread(), 0);
    }
}

#[tokio::test]
async fn launch_enter_that_never_answers_is_reported_as_unknown() {
    // tmux takes the Enter and its call never returns. The call's own limit
    // answers before the start's deadline: the Enter is not repeated, and the
    // reason says that it may have reached the pane. (With less than that
    // limit left the deadline comes first; the flow names the Enter before it
    // sends it, as the clear's test of this shows.)
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_launch();
    let call = starting.start().await;
    progress(&mut starting.events, "up_running", Some("q/lead")).await;
    starting.rig.client.stall_pane_effects(true);
    let result = call.await.unwrap();
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(
                crate::control::reservation::ReservationError::Timeout
            ))
        ),
        "{result:?}"
    );
    assert_eq!(starting.effects(), starting.launched());
    let reason = starting.agent().await.unwrap().reason.unwrap_or_default();
    assert!(
        reason.starts_with(&format!(
            "{START}: running the launch command failed, and whether it reached the pane is unknown"
        )),
        "{reason}"
    );
}

#[tokio::test]
async fn shell_named_harness_is_stuck_not_ready() {
    // After the Enter the pane shows the harness's editor while tmux still
    // names the shell as its foreground program (a wrapper under the shell's
    // name): the harness can't be told from the shell, so the agent is never
    // Ready and nothing more is sent. Also: a harness that is pinned and then
    // gives way to the shell.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_launch();
    starting.frame(Starting::screen(CODEX_FRESH, "zsh"));
    starting.frame(Starting::screen(CODEX_FRESH, "zsh"));
    let result = starting.up().await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(starting.effects(), starting.launched());
    starting.assert_stuck("the launch has started no harness").await;
    assert_eq!(starting.unread(), 0);

    let mut starting = Starting::new(Harness::ClaudeCode).await;
    starting.script_launch();
    starting.frame(Starting::screen(CLAUDE_RUNNING, "claude"));
    starting.frame(starting.staged());
    let result = starting.up().await;
    assert!(
        matches!(&result, Err(AgentError::Refused(reason)) if reason.contains("the harness exited to its shell during the start")),
        "{result:?}"
    );
    starting
        .assert_stuck("the harness exited to its shell during the start")
        .await;
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn up_deadline_is_stuck_with_owner_note() {
    // The harness's editor is up and its session never arrives: the call's
    // 60 seconds pass on the paused clock, the caller gets a timeout, and the
    // agent is Stuck with what the start last waited for.
    let mut starting = Starting::new(Harness::Pi).await;
    starting.script_launch();
    starting.frame(Starting::screen(PI_PADDED, "pi"));
    let result = starting.up().await;
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(
                crate::control::reservation::ReservationError::Timeout
            ))
        ),
        "{result:?}"
    );
    starting
        .assert_stuck("the harness has not announced its session")
        .await;
    assert_eq!(starting.effects(), starting.launched());
    // Stuck is the end of this start: a second one is refused with the pane kept.
    starting
        .rig
        .client
        .script_inspect(&pane("%0"), Some(&Starting::inspection("%0", None, "zsh")))
        .unwrap();
    let again = starting.up().await;
    assert!(
        matches!(again, Err(AgentError::Organisation(_))),
        "{again:?}"
    );
    assert_eq!(starting.effects(), starting.launched());
}
