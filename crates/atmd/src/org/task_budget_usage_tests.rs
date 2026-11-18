// Synthetic: neutral observations via T4a's real command/ledger, not harness
// payloads. The existing in-memory actor and shared scheduler stay real.

async fn usage_task(rig: &Rig, cost: Option<i64>, turns: Option<u32>) -> TaskId {
    let made = rig.run(OrgCommand::Task(TaskCommand::Create(TaskCreated {
        project_id: None, title: "Metered work".into(), kind: TaskKind::Work, creator: Address::Owner,
        parent: None, judges: None, measure: TaskMeasure::Owner,
        budget: Some(atm_org::TaskBudget {
            cost: cost.map(atm_core::Money::from_microdollars), turns, deadline: None,
        }), files: vec![], rests_on: None,
    }))).await;
    let task_id: TaskId = format!("t{}", made.first().unwrap().seq).parse().unwrap();
    rig.run(OrgCommand::Task(TaskCommand::Assign {
        task_id: task_id.clone(), agent_id: agent("build"),
        by: TaskActor { who: Address::Owner, manager_of: None },
    })).await;
    task_id
}

async fn usage_sample(rig: &Rig, cumulative: Option<i64>, turn: bool) -> Vec<Envelope> {
    rig.run(OrgCommand::ObserveTaskUsage(atm_org::TaskUsageObservation {
        session_id: sid("s1"), pane: Some("%2".into()), turn,
        cumulative_cost: cumulative.map(atm_core::Money::from_microdollars), context_percent: None,
    })).await
}

// Break caught: budgets read only final Task.cost, or need an idle edge/timer
// instead of evaluating the event which advances T4a's running turn count.
#[tokio::test(start_paused = true)]
async fn budget_turn_crossing_uses_committed_live_task_cost() {
    let mut rig = Rig::new().await;
    let task = usage_task(&rig, None, Some(1)).await;
    budgets_only(&rig);
    rig.report(working("s1")).await;
    usage_sample(&rig, None, true).await;
    assert_eq!(rig.state().await.task_cost(&task, rig.now()).unwrap().turns, Some(1));
    let events = usage_sample(&rig, None, true).await;
    let crossing = events.first().unwrap();
    assert_eq!(crossing.kind, "task.usage_observed");
    let (message, note, when) = rig.sent().await;
    assert_eq!(note.task, Some(task.clone()));
    assert_eq!(note.to, Address::Owner);
    assert_eq!(when, at(0));
    assert_eq!(message, MessageId::new(crossing.seq + 1));
    let state = rig.state().await;
    assert_eq!(state.task_cost(&task, rig.now()).unwrap().turns, Some(2));
    assert!(state.task(&task).unwrap().cost.is_none(), "not acceptance-only accounting");
    assert_eq!(state.task(&task).unwrap().state, TaskState::Assigned);
}

// Break caught: session lifetime cost is compared to a task budget, samples
// are summed, or a correction/equal sample invents additional task spend.
#[tokio::test(start_paused = true)]
async fn budget_cost_crossing_reads_the_single_high_water_ledger() {
    let mut rig = Rig::new().await;
    let task = usage_task(&rig, Some(1_000_000), None).await;
    budgets_only(&rig);
    rig.report(working("s1")).await;
    for cost in [5_000_000, 6_000_000, 6_000_000, 5_500_000] {
        usage_sample(&rig, Some(cost), false).await;
    }
    assert_eq!(rig.state().await.task_cost(&task, rig.now()).unwrap().cost,
        Some(atm_core::Money::from_microdollars(1_000_000)));
    usage_sample(&rig, Some(6_000_001), false).await;
    let (_, note, when) = rig.sent().await;
    assert_eq!(note.task, Some(task.clone()));
    assert_eq!(when, at(0));
    assert_eq!(rig.state().await.task_cost(&task, rig.now()).unwrap().cost,
        Some(atm_core::Money::from_microdollars(1_000_001)));
}

// Break caught: a second dimension or later usage/state event rearms a task
// whose first crossed limit already caused its one durable message.
#[tokio::test(start_paused = true)]
async fn budget_later_cost_crossing_does_not_repeat_a_turn_alert() {
    let mut rig = Rig::new().await;
    let task = usage_task(&rig, Some(0), Some(0)).await;
    budgets_only(&rig);
    usage_sample(&rig, Some(0), true).await;
    let (_, note, _) = rig.sent().await;
    assert_eq!(note.task, Some(task.clone()));
    usage_sample(&rig, Some(1), true).await;
    rig.set(&task, TaskState::Working, None, "build").await;
    let sentinel = deadline_task(&rig, Address::Owner, 1).await;
    let (_, note, _) = rig.sent().await;
    assert_eq!(note.task, Some(sentinel));
    assert_eq!(rig.count("msg.sent"), 2);
}

// Break caught: a crossing is lost when acceptance reaches the actor before
// the shared scheduler snapshots, or startup ignores terminal task totals.
#[tokio::test(start_paused = true)]
async fn budget_usage_crossing_is_caught_after_immediate_acceptance() {
    let mut rig = Rig::new().await;
    let task = usage_task(&rig, Some(0), Some(0)).await;
    usage_sample(&rig, Some(0), false).await;
    usage_sample(&rig, Some(1), true).await;
    rig.run(OrgCommand::Task(TaskCommand::Submit {
        task_id: task.clone(), what: atm_org::TaskSubmission::Result, files: vec![],
        by: TaskActor { who: Address::Agent(agent("build")), manager_of: None },
    })).await;
    rig.run(OrgCommand::Task(TaskCommand::Verdict {
        task_id: task.clone(), verdict: atm_org::TaskVerdict::Accepted,
        review_task: None, note: None,
        by: TaskActor { who: Address::Owner, manager_of: None },
    })).await;
    budgets_only(&rig);
    let (_, note, _) = rig.sent().await;
    assert_eq!(note.task, Some(task.clone()));
    assert_eq!(rig.state().await.task(&task).unwrap().state, TaskState::Accepted);
    assert_eq!(rig.count("msg.sent"), 1);
}

// Break caught: running counters restart at zero, a session baseline is
// recharged after replay, or a committed budget note is resent after replay.
#[tokio::test(start_paused = true)]
async fn budget_replayed_usage_catches_up_once_and_preserves_cost_baseline() {
    let rig = Rig::new().await;
    let task = usage_task(&rig, Some(1_000_000), Some(1)).await;
    usage_sample(&rig, Some(5_000_000), true).await;
    usage_sample(&rig, Some(6_000_001), true).await;
    let state = rig.log.lock().unwrap().iter().fold(OrgState::default(), |mut state, envelope| {
        let event = OrgEvent::from_parts(&envelope.kind, &envelope.body).unwrap().unwrap();
        state.apply(envelope.seq, envelope.ts, &event).unwrap();
        state
    });
    let mut after = Rig::over(state, rig.log.clone(), None).await;
    budgets_only(&after);
    let (_, note, _) = after.sent().await;
    assert_eq!(note.task, Some(task.clone()));
    usage_sample(&after, Some(6_000_002), false).await;
    assert_eq!(after.state().await.task_cost(&task, after.now()).unwrap().cost,
        Some(atm_core::Money::from_microdollars(1_000_002)));
    let sentinel = deadline_task(&after, Address::Owner, 1).await;
    let (_, note, _) = after.sent().await;
    assert_eq!(note.task, Some(sentinel));
    assert_eq!(after.count("msg.sent"), 2);
}
