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

/// The same shell once the launch runs: the harness is its foreground job.
fn harness_in_front() -> ProcFacts {
    ProcFacts {
        foreground_group: 150,
        children: 1,
        ..idle_shell()
    }
}

/// (harness, its ready editor, the command tmux shows for it)
const HARNESSES: [(Harness, &str, &str); 3] = [
    (Harness::ClaudeCode, CLAUDE_READY, "claude"),
    (Harness::Pi, PI_PADDED, "pi"),
    (Harness::Codex, CODEX_FRESH, "codex"),
];

mod hiring {
    use super::*;
    use crate::{
        org::reporting_line::ReportingFiles,
        server::methods::{router_with_reporting, CallerPane, MethodError, MethodRouter},
    };
    use atm_org::{Address, DecisionCommand, DecisionId, DecisionKind, DecisionRequest,
        DecisionResolution, DecisionStatus, ProvisionalChoice};

    const ENFORCE: &str = "title='Hiring'\nprojects=['q']\n[hire]\nmode='enforce'\n";
    const TITLE: &str = "Hire q/lead";
    const MANAGER: &str = "%0";
    const BOSS: &str = "%7";
    const STRANGER: &str = "%9";

    struct Fixture {
        start: Starting,
        dir: std::path::PathBuf,
        reporting: ReportingFiles,
    }

    impl Fixture {
        async fn new(rig: Rig) -> Self {
            let start = Starting::with_role(rig, Harness::Codex, "manager='ops/manager'\n").await;
            start.agents.pause_revalidation();
            let ops = start.project.path().join("ops");
            std::fs::create_dir_all(ops.join(".atm/roles")).unwrap();
            start.rig.command(OrgCommand::RegisterProject {
                project_id: "ops".parse().unwrap(), root: ops.to_str().unwrap().into(), move_root: false,
            }).await;
            for (name, pane, manager) in [("manager", MANAGER, "boss"), ("boss", BOSS, "owner"), ("stranger", STRANGER, "owner")] {
                Self::write_role(&ops, name, manager);
                start.rig.command(OrgCommand::Adopt {
                    agent_id: format!("ops/{name}").parse().unwrap(), pane: pane.into(), session_id: None,
                }).await;
            }
            let dir = start.project.path().join("departments");
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("enforce.toml"), ENFORCE).unwrap();
            start.rig.client.mock().clear_calls();
            Self { start, dir, reporting: ReportingFiles::default() }
        }

        fn write_role(root: &std::path::Path, name: &str, manager: &str) {
            std::fs::write(root.join(format!(".atm/roles/{name}.md")),
                format!("+++\nharness='codex'\nmanager='{manager}'\n+++\nPRIVATE body\n")).unwrap();
        }

        fn root(&self) -> std::path::PathBuf {
            self.start.params().project_root
        }

        fn role_path(&self) -> String {
            self.root().canonicalize().unwrap().join(".atm/roles/lead.md").to_str().unwrap().into()
        }

        fn router(&self) -> MethodRouter {
            let mut services = MethodServices::new(self.start.rig.registry.clone());
            services.org = self.start.rig.org.clone();
            services.agents = Some(self.start.agents.clone());
            services.departments_dir = Some(self.dir.clone());
            router_with_reporting(services, self.reporting.clone()).unwrap()
        }

        fn request(&self, kind: DecisionKind, title: &str, path: String) -> DecisionRequest {
            DecisionRequest {
                requester: "ops/manager".parse::<AgentId>().map(Address::Agent).unwrap(),
                kind, title: title.into(), context: "Synthetic hire history".into(), files: vec![path],
                options: vec!["approve".into(), "decline".into()], recommended: None,
                may_be_undone: true, changes_intent: true, deadline: None,
            }
        }

        async fn record(&self, request: DecisionRequest) -> DecisionId {
            let events = self.start.rig.org.execute(OrgCommand::Decision(DecisionCommand::Request(request)), chrono::Utc::now()).await.unwrap();
            DecisionId::new(events[0].seq)
        }

        async fn resolve(&self, id: DecisionId, option: &str) {
            self.start.rig.command(OrgCommand::Decision(DecisionCommand::Resolve(DecisionResolution {
                decision_id: id, by: Address::Owner, option: option.into(), note: None, standing_rule: false,
            }))).await;
        }

        async fn call(&self, caller: Option<&str>, hire: Option<DecisionId>) -> Result<Value, MethodError> {
            up(self.router(), self.root(), caller, hire).await
        }

        async fn count(&self) -> usize {
            self.start.rig.org.snapshot().await.unwrap().decisions().count()
        }
    }

    async fn up(router: MethodRouter, root: std::path::PathBuf, pane: Option<&str>, hire: Option<DecisionId>) -> Result<Value, MethodError> {
        let mut params = json!({"name":"lead", "project_root":root});
        if let Some(id) = hire {
            params["hire"] = json!(id);
        }
        let caller = CallContext { from_pane: pane.map(|p| CallerPane(p.into())), ..caller() };
        router.call(caller, "agent.up", params).await
    }

    // Break caught: placement reads or a lifecycle write happen on a refusal,
    // or the durable decision loses its effective role/caller on registration.
    #[tokio::test]
    async fn hire_refusal_precedes_all_tmux_and_agent_events() {
        for registration in ["new", "same", "moved"] {
            let f = Fixture::new(Rig::new().await).await;
            std::fs::write(f.root().join(".atm/project.toml"), "id='q'\n").unwrap();
            if registration != "new" {
                let root = if registration == "same" { f.root() } else { f.start.project.path().join("old") };
                f.start.rig.command(OrgCommand::RegisterProject {
                    project_id: "q".parse().unwrap(), root: root.to_str().unwrap().into(), move_root: false,
                }).await;
            }
            f.start.rig.client.mock().set_panes(vec![]);
            let mut log = f.start.rig.org.committed.subscribe();
            let error = f.call(Some(MANAGER), None).await.unwrap_err().0;
            assert_eq!(error.code, "refused", "{error:?}");
            let state = f.start.rig.org.snapshot().await.unwrap();
            let decision = state.decisions().next().unwrap();
            assert_eq!(error.message, format!("agent q/lead was not started: department enforce: not on roster; hire decision {} is pending", decision.id));
            assert_eq!(decision.status, DecisionStatus::Pending);
            assert_eq!(decision.request.requester.to_string(), "ops/manager");
            assert_eq!(decision.request.title, TITLE);
            assert_eq!(decision.request.kind, DecisionKind::Hire);
            assert_eq!(decision.request.options, ["approve", "decline"]);
            assert_eq!(decision.request.files, [f.role_path()]);
            assert!(decision.request.recommended.is_none() && decision.request.deadline.is_none());
            for required in ["enforce", "not on roster", "ops/manager", "(default)"] {
                assert!(decision.request.context.contains(required), "{:?}", decision.request);
            }
            assert!(!decision.request.context.contains("PRIVATE"));
            assert!(state.agent(&started()).is_none());
            let mut expected = vec![];
            if registration != "same" { expected.push("project.registered"); }
            expected.push("decision.requested");
            assert_eq!(committed_kinds(&mut log), expected);
            assert!(f.start.rig.client.mock().calls().is_empty());
        }
    }

    // Break caught: producing two questions, losing one blocker, or sending
    // the off-list candidate to placement before asking.
    #[tokio::test]
    async fn hire_model_and_roster_misses_share_one_card() {
        let f = Fixture::new(Rig::new().await).await;
        let role = "+++\nharness='codex'\nmanager='ops/manager'\nmodel='test-pro:max'\n+++\nPRIVATE body\n";
        std::fs::write(f.role_path(), role).unwrap();
        std::fs::write(
            f.dir.join("enforce.toml"),
            format!("{ENFORCE}[spend]\nmodels=['test-lite*']\n"),
        ).unwrap();
        let mut log = f.start.rig.org.committed.subscribe();
        let error = f.call(Some(MANAGER), None).await.unwrap_err().0;
        let state = f.start.rig.org.snapshot().await.unwrap();
        let decision = state.decisions().next().unwrap();
        let reasons = "department enforce: not on roster; model \"test-pro:max\" is not allowed";
        assert_eq!(error.code, "refused");
        assert_eq!(error.message, format!("agent q/lead was not started: {reasons}; hire decision {} is pending", decision.id));
        assert_eq!(decision.request.kind, DecisionKind::Hire);
        assert_eq!(decision.request.title, TITLE);
        assert_eq!(decision.request.files, [f.role_path()]);
        assert_eq!(decision.request.options, ["approve", "decline"]);
        assert!(decision.request.context.contains(reasons), "{:?}", decision.request);
        assert!(!decision.request.context.contains("PRIVATE"));
        assert_eq!(state.decisions().count(), 1);
        assert_eq!(committed_kinds(&mut log), ["project.registered", "decision.requested"]);
        assert!(state.agent(&started()).is_none());
        assert!(f.start.rig.client.mock().calls().is_empty());
    }

    // Break caught: a large but readable policy or long escaped model makes
    // the decision context exceed its bound or drops any department's reason.
    #[tokio::test]
    async fn hire_full_scan_fits_one_decision() {
        let f = Fixture::new(Rig::new().await).await;
        std::fs::write(f.role_path(), format!("+++\nharness='codex'\nmanager='ops/manager'\nmodel=\"{}\"\n+++\n", "\\u202e".repeat(600))).unwrap();
        std::fs::remove_file(f.dir.join("enforce.toml")).unwrap();
        for i in 0..32 {
            std::fs::write(f.dir.join(format!("dept-{i:027}.toml")), format!("{ENFORCE}[spend]\nmodels=[]\n")).unwrap();
        }
        let error = f.call(Some(MANAGER), None).await.unwrap_err().0;
        let state = f.start.rig.org.snapshot().await.unwrap();
        let card = state.decisions().next().unwrap();
        let context = &card.request.context;
        for text in [context.as_str(), error.message.as_str()] {
            assert_eq!(text.matches("not on roster; model").count(), 32);
            assert_eq!(text.matches(" is not allowed").count(), 32);
            assert!(text.is_ascii() && !text.contains(char::is_control));
        }
        assert!(card.request.title.len() + 2 + context.len() < 16 * 1024);
        assert!(error.message.ends_with(&format!("hire decision {} is pending", card.id)));
        assert_eq!(state.decisions().count(), 1);
    }

    // Break caught: changed reasons replace a pending hire, or a changed role
    // body invalidates an otherwise valid approval for the same seat/path.
    #[tokio::test]
    async fn hire_reuses_roster_card_for_a_later_model_miss() {
        let f = Fixture::new(Rig::new().await).await;
        let role = "+++\nharness='codex'\nmanager='ops/manager'\nmodel='test-pro:max'\n+++\nPRIVATE body\n";
        std::fs::write(f.role_path(), role).unwrap();
        f.start.rig.client.mock().set_panes(vec![]);
        let first = f.call(Some(MANAGER), None).await.unwrap_err().0;
        assert!(first.message.contains("department enforce: not on roster"), "{first:?}");
        let before = f.start.rig.org.snapshot().await.unwrap();
        let pending = before.decisions().next().unwrap();
        std::fs::write(
            f.dir.join("enforce.toml"),
            format!("{}[spend]\nmodels=['test-lite*']\n", ENFORCE.replace("[hire]", "roster=['q/lead']\n[hire]")),
        ).unwrap();
        let next = f.call(Some(MANAGER), None).await.unwrap_err().0;
        assert_eq!(next.message, format!("agent q/lead was not started: department enforce: model \"test-pro:max\" is not allowed; hire decision {} is pending", pending.id));
        let after = f.start.rig.org.snapshot().await.unwrap();
        assert_eq!(after.decisions().count(), 1);
        assert_eq!(after.decisions().next().unwrap().request.context, pending.request.context);
        assert!(f.start.rig.client.mock().calls().is_empty());

        f.resolve(pending.id, "approve").await;
        std::fs::write(f.role_path(), role.replace("test-pro:max", "test-other")).unwrap();
        let unauthorized = f.call(Some(STRANGER), Some(pending.id)).await.unwrap_err().0;
        assert!(unauthorized.message.contains("manager"), "{unauthorized:?}");
        assert!(f.start.rig.client.mock().calls().is_empty());
        let placement = f.call(Some(MANAGER), Some(pending.id)).await.unwrap_err().0;
        assert!(placement.message.contains("no observation script for %0"), "{placement:?}");
        assert!(!f.start.rig.client.mock().calls().is_empty());
        assert_eq!(f.count().await, 1);
    }

    // Break caught: checking only the immediate manager, allowing a broken
    // ancestor, or using an approval in place of current caller authority.
    #[tokio::test]
    async fn hire_requires_the_current_manager_line() {
        let f = Fixture::new(Rig::new().await).await;
        f.start.rig.client.mock().set_panes(vec![]);
        let token = f.record(f.request(DecisionKind::Hire, TITLE, f.role_path())).await;
        f.resolve(token, "approve").await;
        for (pane, manager) in [(STRANGER, "ops/manager"), (MANAGER, "owner"), (MANAGER, "ops/bad?"), (MANAGER, "ops/unknown"), (MANAGER, "lead")] {
            Fixture::write_role(&f.root(), "lead", manager);
            let error = f.call(Some(pane), Some(token)).await.unwrap_err().0;
            assert!(error.message.contains("manager"), "{manager}: {error:?}");
            assert!(f.start.rig.client.mock().calls().is_empty());
        }
        Fixture::write_role(&f.root(), "lead", "ops/manager");
        let ops = f.start.project.path().join("ops");
        for boss_manager in ["manager", "q/lead"] {
            // q/lead is a recorded Down seat, so it is eligible for Up.
            if boss_manager == "q/lead" {
                f.start.rig.command(OrgCommand::Up {
                    agent_id: started(), role: atm_org::parse_role("lead", "+++\nharness='codex'\n+++\n").unwrap(),
                }).await;
                f.start.rig.command(OrgCommand::Down { agent_id: started(), reason: "fixture".into() }).await;
            }
            Fixture::write_role(&ops, "boss", boss_manager);
            let error = f.call(Some(MANAGER), None).await.unwrap_err().0;
            assert!(error.message.contains("cycle"), "{error:?}");
        }
        Fixture::write_role(&ops, "boss", "owner");
        f.reporting.slot.close();
        let error = f.call(Some(MANAGER), None).await.unwrap_err().0;
        assert!(error.message.contains("reader stopped"), "{error:?}");
        assert!(f.start.rig.client.mock().calls().is_empty());
        assert_eq!(f.count().await, 1, "authority failure did not request a hire");
        for (pane, cause) in [(None, "no observation script for %0"), (Some("%80"), "no observation script for %80")] {
            let error = f.call(pane, None).await.unwrap_err().0;
            assert!(error.message.contains(cause), "owner bypass: {error:?}");
            assert!(!f.start.rig.client.mock().calls().is_empty());
        }
        assert_eq!(f.count().await, 1);
    }

    // Break caught: an already-up seat reaches placement or requests a hire.
    #[tokio::test]
    async fn hire_refuses_already_up_without_a_decision() {
        let f = Fixture::new(Rig::new().await).await;
        f.start.rig.client.mock().set_panes(vec![]);
        // This owner call registers q even though there is no place to split it.
        f.call(None, None).await.unwrap_err();
        f.start.rig.command(OrgCommand::Adopt {
            agent_id: started(), pane: "%5".into(), session_id: None,
        }).await;
        f.start.rig.client.mock().clear_calls();
        let error = f.call(Some(MANAGER), None).await.unwrap_err().0;
        assert!(error.message.contains("while Starting"), "{error:?}");
        assert_eq!(f.count().await, 0);
        assert!(f.start.rig.client.mock().calls().is_empty());
    }

    // Break caught: a dropped reply or another authorized caller loses the
    // pending card; old attachments must be reported, not silently replaced.
    #[tokio::test]
    async fn hire_reuses_pending_request_after_a_dropped_reply() {
        let (gate, mut receipts) = mpsc::channel(1);
        let f = Fixture::new(Rig::gated(Some(("decision.requested", gate))).await).await;
        f.start.rig.client.mock().set_panes(vec![]);
        let released = {
            let call = f.call(Some(MANAGER), None);
            tokio::pin!(call);
            tokio::select! {
                result = &mut call => panic!("expected decision append before reply: {result:?}"),
                receipt = receipts.recv() => receipt.unwrap(),
            }
        }; // Lose the response while the commit is in flight.
        released.send(()).unwrap();
        let state = f.start.rig.org.snapshot().await.unwrap();
        let pending = state.decisions().next().unwrap();
        let retry = f.call(Some(BOSS), None).await.unwrap_err().0;
        assert!(retry.message.contains(&format!("hire decision {} is pending", pending.id)), "{retry:?}");
        assert_eq!(f.count().await, 1);
        assert!(f.start.rig.client.mock().calls().is_empty());

        let f = Fixture::new(Rig::new().await).await;
        f.start.rig.client.mock().set_panes(vec![]);
        let old = f.record(f.request(DecisionKind::Hire, TITLE, "/old/.atm/roles/lead.md".into())).await;
        let retry = f.call(Some(MANAGER), None).await.unwrap_err().0;
        for part in [old.to_string().as_str(), "attachment", "withdraw"] {
            assert!(retry.message.contains(part), "{retry:?}");
        }
        assert_eq!(f.count().await, 1);
        assert!(f.start.rig.client.mock().calls().is_empty());
    }

    // Break caught: a token is treated as approval based on only its id,
    // status, title, or path, or can override authority. Use the real writer.
    #[tokio::test]
    async fn hire_checks_explicit_approval_on_the_fold() {
        let f = Fixture::new(Rig::new().await).await;
        f.start.rig.client.mock().set_panes(vec![]);
        // A token still has to be valid if the roster independently allows Up.
        std::fs::write(f.dir.join("enforce.toml"), ENFORCE.replace("[hire]", "roster=['q/lead']\n[hire]")).unwrap();
        let unknown = DecisionId::new(9999);
        assert_eq!(f.call(Some(MANAGER), Some(unknown)).await.unwrap_err().0.code, "refused");
        for (kind, title, path, option) in [
            (DecisionKind::Action, TITLE, f.role_path(), Some("approve")),
            (DecisionKind::Hire, "Hire other/lead", f.role_path(), Some("approve")),
            (DecisionKind::Hire, TITLE, "/old/.atm/roles/lead.md".into(), Some("approve")),
            (DecisionKind::Hire, TITLE, f.role_path(), Some("decline")),
            (DecisionKind::Hire, TITLE, f.role_path(), None),
        ] {
            let d = f.record(f.request(kind, title, path)).await;
            if let Some(value) = option { f.resolve(d, value).await; }
            let before = f.count().await;
            let error = f.call(Some(MANAGER), Some(d)).await.unwrap_err().0;
            assert_eq!(error.code, "refused", "{error:?}");
            assert!(error.message.contains(&d.to_string()), "{error:?}");
            assert_eq!(f.count().await, before);
        }
        // Inject durable historical state via the internal writer, not the RPC:
        // today's Hire policy requires waiting and doesn't grant a relay path.
        for relayed_by in [None, Some("ops/boss".parse().unwrap())] {
            let mut request = f.request(DecisionKind::Hire, TITLE, f.role_path());
            request.recommended = Some("approve".into());
            let d = f.record(request).await;
            f.start.rig.command(OrgCommand::Decision(DecisionCommand::TakeProvisionally(ProvisionalChoice {
                decision_id: d, option: "approve".into(), by: "ops/manager".parse().unwrap(), relayed_by,
            }))).await;
            for resolve in [false, true] {
                if resolve { f.resolve(d, "approve").await; }
                let error = f.call(Some(MANAGER), Some(d)).await.unwrap_err().0;
                assert_eq!(error.code, "refused", "{error:?}");
            }
        }
        assert!(f.start.rig.client.mock().calls().is_empty());
        std::fs::write(f.dir.join("enforce.toml"), ENFORCE).unwrap();
        let token = f.record(f.request(DecisionKind::Hire, TITLE, f.role_path())).await;
        f.resolve(token, "approve").await;
        let error = f.call(Some(BOSS), Some(token)).await.unwrap_err().0;
        assert!(error.message.contains("no observation script for %7"), "{error:?}");
        assert!(!f.start.rig.client.mock().calls().is_empty());
    }

    // Break caught: entering the daemon's authority or token gate for a
    // report-only project changes what a named caller can start.
    #[tokio::test]
    async fn hire_report_only_matches_missing_directory() {
        let unknown = DecisionId::new(9999);
        let legacy_ready = json!({"agent_id": "q/lead", "pane": "%1", "state": "ready"});
        for (report_only, hire) in [
            (false, None),
            (true, None),
            (false, Some(unknown)),
            (true, Some(unknown)),
        ] {
            let case = format!("report_only={report_only}, hire={hire:?}");
            let mut f = Fixture::new(Rig::new().await).await;
            Fixture::write_role(&f.root(), "lead", "owner");
            std::fs::remove_file(f.dir.join("enforce.toml")).unwrap();
            if report_only {
                std::fs::write(
                    f.dir.join("report.toml"),
                    "title='Hiring report'\nprojects=['q']\n[hire]\nmode='report'\n[spend]\nmodels=['test-lite*']\n",
                )
                .unwrap();
            } else {
                std::fs::remove_dir(&f.dir).unwrap();
            }

            f.start.script_launch();
            f.start.frame(f.start.staged());
            f.start.frame(Starting::screen(CODEX_FRESH, "codex"));
            f.start.follow_up();
            f.start.follow_up();
            let mut request = tokio::spawn(up(f.router(), f.root(), Some(MANAGER), hire));
            // Policy reads use the real clock; only the scripted launch is paused.
            tokio::select! {
                biased;
                () = progress(&mut f.start.events, "up_prepared", Some("q/lead")) => {},
                reply = &mut request => panic!("{case}: start returned before preparation: {reply:?}"),
            }
            tokio::time::pause();
            let reply = request.await.unwrap();
            tokio::time::resume();

            assert_eq!(reply.unwrap_or_else(|error| panic!("{case}: {error:?}")), legacy_ready, "{case}");
            let state = f.start.rig.org.snapshot().await.unwrap();
            assert_eq!(
                state.agent_for_pane(MANAGER).unwrap().id.to_string(),
                "ops/manager",
                "{case}"
            );
            assert_eq!(state.agent(&started()).unwrap().status, AgentStatus::Ready, "{case}");
            assert_eq!(state.decisions().count(), 0, "{case}");
        }
    }

    // Break caught: a bad neighbor disables enforcement, or a successful
    // fail-open launch forgets to give its named caller the warning.
    #[tokio::test]
    async fn hire_department_read_failures_warn_without_overriding_known_enforcement() {
        let mut f = Fixture::new(Rig::new().await).await;
        std::fs::write(f.dir.join("bad.toml"), "title=17\n").unwrap();
        let error = f.call(Some(MANAGER), None).await.unwrap_err().0;
        assert!(error.message.contains("hire decision"), "{error:?}");
        assert!(f.start.rig.client.mock().calls().is_empty());
        std::fs::remove_file(f.dir.join("enforce.toml")).unwrap();
        f.start.script_launch();
        f.start.frame(f.start.staged());
        f.start.frame(Starting::screen(CODEX_FRESH, "codex"));
        f.start.follow_up();
        f.start.follow_up();
        let call = up(f.router(), f.root(), Some(MANAGER), None);
        let mut request = tokio::spawn(call);
        // Wait for real filesystem preparation and all policy reads before
        // pausing only the scripted lifecycle clock.
        tokio::select! {
            () = progress(&mut f.start.events, "up_prepared", Some("q/lead")) => {},
            result = &mut request => panic!("start returned before preparation: {result:?}"),
        }
        tokio::time::pause();
        let result = request.await.unwrap().unwrap();
        assert_eq!(result, json!({"agent_id":"q/lead", "pane":"%1", "state":"ready",
            "warning":"department policy incomplete; hiring was not checked for q"}));
    }
}

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
        // The launch's Enter runs the harness: from then on the pane's shell
        // is not idle, it holds the harness as its foreground job.
        *rig.proc_after_enter.lock().unwrap() = Some(harness_in_front());
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

    /// The frames a start would need to read the shell's line a second time
    /// as its prompt and launch after it: `line` five times (a refused paste
    /// guard, the start's own look, two captures of a new proof, a paste
    /// guard), then `line` with the launch text after it, twice (the staged
    /// capture and the Enter's guard), and the harness.
    fn script_second_reading(&self, line: &str) {
        for _ in 0..5 {
            self.frame(Self::shell(line));
        }
        let staged = Self::shell(&format!("{line}{}", self.text));
        self.frame(staged.clone());
        self.frame(staged);
        self.frame(Self::screen(CODEX_FRESH, "codex"));
    }

    /// What is left of [`Self::script_second_reading`] when the start ends
    /// at its own look: six frames, three reads each.
    const SECOND_READING_UNREAD: usize = 6 * 3;

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
            hire: None,
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
        // Only an operation that ends Ready queues a start line.
        assert!(
            state
                .messages()
                .all(|message| !crate::org::start_line::is_start_line(&message.content)),
            "a start line after an operation that did not end Ready"
        );
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
                // The start line, queued once the agent is Ready.
                "msg.sent",
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
        // The binding, then the start line.
        assert_eq!(
            committed_kinds(&mut committed),
            ["agent.session_bound", "msg.sent"]
        );
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
    // tmux answered the split with something that names no pane: whether it
    // made one is not known, and the reason does not say that there is none.
    assert_down_without_pane(
        &mut starting,
        "its pane may have been created and is not recorded",
    )
    .await;

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

const SELECTED: &str = "the new pane was selected before its prompt could be read, so nothing was typed; start the harness there by hand and adopt it, or take the agent down and up again";
const UNREAD_FOCUS: &str = "whether the new pane was selected could not be read before its prompt was read, so nothing was typed; start the harness there by hand and adopt it, or take the agent down and up again";
const CHANGED: &str = "the shell's line changed after its prompt was read, so nothing was typed";

#[tokio::test]
async fn op7_shell_prefix_before_first_capture_is_accepted_as_the_prompt() {
    // Characterized limit, not permission to overwrite user input: an earlier
    // focus excursion is absent from both captures. The shell prefix is then
    // indistinguishable from a prompt. Twin: launch_line_with_typed_prefix_sends_no_enter
    // refuses a prefix introduced AFTER the baseline has been read.
    // Synthetic: a harmless shell prefix stable before the first capture.
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        let before = format!("{PROMPT}printf earlier-input; ");
        for _ in 0..3 {
            starting.frame(Starting::shell(&before));
        }
        let staged = Starting::shell(&format!("{before}{}", starting.text));
        starting.frame(staged.clone());
        starting.frame(staged);
        starting.frame(Starting::screen(CODEX_FRESH, "codex"));
        let result = starting.up().await.unwrap();
        assert_eq!(result.state, AgentStatus::Ready);
        assert_eq!(starting.effects(), starting.launched());
        assert_eq!(starting.unread(), 0);
    })
    .await
    .expect("pre-capture shell-prefix characterization exceeded its hang guard");
}

#[tokio::test]
async fn focus_before_the_prompt_is_read_ends_the_start_with_nothing_typed() {
    // Text reaches the new pane's line only from a client that selected the
    // pane. A focus seen before the prompt is read voids the reading for this
    // start: the flow does not wait for the focus to leave and then trust the
    // line. The frames are the Task 6 review's two sequences: the owner is in
    // the pane (seen by the second capture of the first pass, or by its
    // first), types `rm -rf `, and leaves; the launch would then be typed
    // after his text and run with it. The later frames are scripted and not
    // asserted read or unread: an observation of the stuck agent may read them.
    // Twin: up_order_is_split_tag_pane_created_paste_enter_ready.
    // (The third: the owner has left again by the second capture.)
    for (first, second) in [(false, true), (true, true), (true, false)] {
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.frame(Starting::shell(PROMPT).with_focus(Some(first)));
        starting.frame(Starting::shell(PROMPT).with_focus(Some(second)));
        let typed = format!("{PROMPT}rm -rf ");
        for _ in 0..3 {
            starting.frame(Starting::shell(&typed));
        }
        let staged = Starting::shell(&format!("{typed}{}", starting.text));
        starting.frame(staged.clone());
        starting.frame(staged);
        starting.frame(Starting::screen(CODEX_FRESH, "codex"));
        let result = starting.up().await;
        assert!(
            matches!(&result, Err(AgentError::Refused(reason)) if reason.contains(SELECTED)),
            "{result:?}"
        );
        assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
        starting.assert_stuck(SELECTED).await;
        assert_told_stuck(&result, &starting.agent().await.unwrap());
    }
}

#[tokio::test]
async fn unknown_focus_before_the_prompt_is_read_ends_the_start_with_nothing_typed() {
    // Unknown focus is not permission. While the client list could not be
    // read the owner may be in the pane, typing; when focus reads "nobody"
    // again, the line with his text would be proven as the prompt. So a
    // capture of the proof whose focus could not be read ends the start, as
    // a shown focus does. The frames are the second review's two sequences
    // (unknown on the first capture; on the second), with the typed line and
    // the launch after it, which are scripted and not asserted read or
    // unread. Twin: up_order_is_split_tag_pane_created_paste_enter_ready.
    for (first, second) in [(None, Some(false)), (Some(false), None)] {
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.frame(Starting::shell(PROMPT).with_focus(first));
        starting.frame(Starting::shell(PROMPT).with_focus(second));
        let typed = format!("{PROMPT}rm -rf ");
        for _ in 0..3 {
            starting.frame(Starting::shell(&typed));
        }
        let staged = Starting::shell(&format!("{typed}{}", starting.text));
        starting.frame(staged.clone());
        starting.frame(staged);
        starting.frame(Starting::screen(CODEX_FRESH, "codex"));
        let result = starting.up().await;
        assert!(
            matches!(&result, Err(AgentError::Refused(reason)) if reason.contains(UNREAD_FOCUS)),
            "{result:?}"
        );
        assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
        starting.assert_stuck(UNREAD_FOCUS).await;
    }
}

#[tokio::test]
async fn launch_refuses_focused_pane() {
    // Focused after the prompt was read, at the paste's own guard: refused
    // there and waited on while it lasts, and so is a focus that can't be
    // read. The line still reads the prompt when the focus has gone, so the
    // launch goes on. (Before the prompt is read either ends the start:
    // focus_before_the_prompt_is_read_ends_the_start_with_nothing_typed and
    // unknown_focus_before_the_prompt_is_read_ends_the_start_with_nothing_typed.)
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    // (guard, the flow's look, guard, look, guard)
    for focus in [Some(true), None, None, Some(true), None] {
        starting.frame(Starting::shell(PROMPT).with_focus(focus));
    }
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
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
    // Each case builds its frame from the start it runs in: the launch text
    // names that start's own directory.
    type Frame = fn(&Starting) -> PaneCapture;
    let not_staged =
        format!("{NOT_RUN}: the shell's line is not its prompt followed by the command");
    let other_program =
        format!("{NOT_RUN}: the pane's shell is not known to be its foreground program");
    let cases: [(&str, Frame, &String); 7] = [
        (
            "typed before the paste",
            |s| Starting::shell(&format!("{PROMPT}rm -rf ~; {}", s.text)),
            &not_staged,
        ),
        (
            "typed after the paste",
            |s| Starting::shell(&format!("{PROMPT}{} --yolo", s.text)),
            &not_staged,
        ),
        (
            "the prompt was drawn again",
            |s| Starting::shell(&format!("pb (main)> {}", s.text)),
            &not_staged,
        ),
        // The right length, and only one of the two parts right: the line
        // is compared whole, not by its length and one end.
        (
            "another prompt of the same length",
            |s| Starting::shell(&format!("pc> {}", s.text)),
            &not_staged,
        ),
        (
            "another command of the same length",
            |s| {
                let mut other = s.text.clone();
                other.replace_range(..1, "x");
                assert_ne!(other, s.text);
                Starting::shell(&format!("{PROMPT}{other}"))
            },
            &not_staged,
        ),
        (
            "another root process",
            |s| {
                let mut capture = s.staged();
                capture.inspection.pane.pane_pid = 200;
                capture
            },
            &other_program,
        ),
        (
            "another program in front",
            |s| {
                let mut capture = s.staged();
                capture.inspection.foreground_command = Some("vim".into());
                capture
            },
            &other_program,
        ),
    ];
    for (label, frame, reason) in cases {
        // Seen by the flow's own capture.
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.script_paste();
        starting.frame(frame(&starting));
        let result = starting.up().await;
        assert!(result.is_err(), "{label}: {result:?}");
        assert_eq!(starting.effects().len(), 3, "{label}: {:?}", starting.effects());
        starting.assert_stuck(reason).await;
        assert_eq!(starting.unread(), 0, "{label}");

        // Staged for the flow, changed at the Enter's own guard.
        let mut starting = Starting::new(Harness::Codex).await;
        starting.script_pane();
        starting.script_paste();
        starting.frame(starting.staged());
        starting.frame(frame(&starting));
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
async fn line_changed_after_the_paste_waits_and_only_the_exact_line_is_run() {
    // After the paste the launch text is on the line and can't be taken
    // back. A line that then reads anything but the prompt followed by the
    // command gets no Enter, and the start waits: when the line reads
    // exactly that again (what was typed after it was removed), the Enter
    // goes out. What runs is the prompt's line and the command, nothing else.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.script_paste();
    starting.frame(Starting::shell(&format!("{PROMPT}{} --yolo", starting.text)));
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched());
    assert_eq!(starting.unread(), 0);

    // The line never reads that again: no Enter, and the start's deadline
    // ends it with the command left typed at the prompt (the changed line
    // for the rest of the sixty seconds, a paced capture each quarter second
    // on the paused clock).
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.script_paste();
    for _ in 0..260 {
        starting.frame(Starting::shell(&format!("{PROMPT}{} --yolo", starting.text)));
    }
    let result = starting.up().await;
    assert_told_stuck(&result, &starting.agent().await.unwrap());
    assert_eq!(starting.effects().len(), 3, "{:?}", starting.effects());
    starting
        .assert_stuck(&format!(
            "{NOT_RUN}: the shell's line is not its prompt followed by the command"
        ))
        .await;
}

#[tokio::test]
async fn prompt_that_reads_differently_after_it_was_read_ends_the_start() {
    // The prompt is read once. Drawn again between that reading and the
    // paste's guard, the line is not read as a new prompt: nothing was sent,
    // and the start ends. (Before the Task 6 review the new line was taken;
    // a line with typed text was taken the same way.)
    //
    // The frames go on as a second reading of the prompt would need them:
    // the new line five times in all, then that line with the launch after
    // it. The start reads two of them (the refused guard, and its own look)
    // and none of the rest.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    starting.script_second_reading("pb (main)> ");
    let result = starting.up().await;
    assert!(result.is_err(), "{result:?}");
    // (An observation of the stuck agent may have inspected the pane once.)
    assert!(
        starting.unread() + 1 >= Starting::SECOND_READING_UNREAD,
        "{}",
        starting.unread()
    );
    assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
    starting.assert_stuck(CHANGED).await;
}

#[tokio::test]
async fn draft_at_the_prompt_before_the_launch_paste_sends_nothing() {
    // The plan's draft_appears_before_guarded_paste_sends_nothing, for a launch
    // (the clear has a test of that name).
    // Something is typed at the prompt after the prompt was read: the paste's
    // guard refuses, the flow's next capture reads another line than the
    // prompt, and the start ends with no paste and no Enter.
    // Twin: up_order_is_split_tag_pane_created_paste_enter_ready.
    // The frames go on as a second reading of the prompt would need them
    // (prompt_that_reads_differently_after_it_was_read_ends_the_start).
    let draft = format!("{PROMPT}rm -rf ~; ");
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    starting.script_second_reading(&draft);
    let result = starting.up().await;
    assert!(result.is_err(), "{result:?}");
    // (An observation of the stuck agent may have inspected the pane once.)
    assert!(
        starting.unread() + 1 >= Starting::SECOND_READING_UNREAD,
        "{}",
        starting.unread()
    );
    assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
    starting.assert_stuck(CHANGED).await;

    // Typed while somebody is focused on the pane: that only waits. When
    // the focus has gone the line is not the prompt, and the start ends.
    let focused = || Starting::shell(&draft).with_focus(Some(true));
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    starting.frame(focused());
    starting.frame(focused());
    starting.frame(Starting::shell(&draft));
    starting.frame(Starting::shell(&draft));
    let result = starting.up().await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(starting.effects().len(), 2, "{:?}", starting.effects());
    starting.assert_stuck(CHANGED).await;
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
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
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
    assert_told_stuck(&result, &starting.agent().await.unwrap());
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
async fn deadline_during_the_launch_enter_says_the_command_may_have_run() {
    // Somebody is focused on the new pane for forty of the start's sixty
    // seconds, after the prompt was read (a refused paste guard and the
    // flow's look at the pane, a paced capture each quarter second on the
    // paused clock), then leaves. The command is typed, and its Enter goes
    // out with less time left than the call's own limit; tmux never answers
    // it. The start's deadline ends the flow while the Enter is out, and the
    // reason names the Enter, not the step before it. (With more time left
    // the call's limit answers first:
    // launch_enter_that_never_answers_is_reported_as_unknown.)
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_pane();
    starting.frame(Starting::shell(PROMPT));
    starting.frame(Starting::shell(PROMPT));
    for _ in 0..160 {
        starting.frame(Starting::shell(PROMPT).with_focus(Some(true)));
    }
    starting.frame(Starting::shell(PROMPT));
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    let call = starting.start().await;
    progress_within(&mut starting.events, "up_running", Some("q/lead"), 59).await;
    starting.rig.client.stall_pane_effects(true);
    let result = call.await.unwrap();
    assert_told_stuck(&result, &starting.agent().await.unwrap());
    assert_eq!(starting.effects(), starting.launched());
    starting
        .assert_stuck("the launch command was being run and may have started the harness")
        .await;
    assert_eq!(starting.unread(), 0);
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
    // 60 seconds pass on the paused clock, the agent is Stuck with what the
    // start last waited for, and the caller is told that reason, not a bare
    // timeout.
    let mut starting = Starting::new(Harness::Pi).await;
    starting.script_launch();
    starting.frame(Starting::screen(PI_PADDED, "pi"));
    let result = starting.up().await;
    assert_told_stuck(&result, &starting.agent().await.unwrap());
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
