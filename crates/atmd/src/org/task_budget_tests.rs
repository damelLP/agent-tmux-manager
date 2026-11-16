// Budget tests share T3's in-memory actor/registry rig, never real socket I/O.

async fn deadline_task(rig: &Rig, creator: Address, deadline: i64) -> TaskId {
    let made = rig.run(OrgCommand::Task(TaskCommand::Create(TaskCreated {
        title: "Budgeted work".into(), kind: TaskKind::Work, creator,
        parent: None, judges: None, measure: TaskMeasure::Owner,
        budget: Some(atm_org::TaskBudget { cost: None, turns: None, deadline: Some(at(deadline)) }),
        files: vec![], rests_on: None,
    }))).await;
    format!("t{}", made.first().unwrap().seq).parse().unwrap()
}

fn budgets_only(rig: &Rig) -> tokio::task::JoinHandle<()> {
    let off = parse_reminder_settings("[reminder]\nenabled=false").unwrap();
    tokio::spawn(run(rig.org.clone(), rig.rows.subscribe(), off, rig.clock(), rig.cancel.clone()))
}

// Break caught: deadlines depend on a registry turn end instead of their timer.
#[tokio::test(start_paused = true)]
async fn budget_deadline_wakes_without_a_registry_turn() {
    let mut rig = Rig::new().await;
    let task = deadline_task(&rig, Address::Owner, 30).await;
    let before = rig.state().await.task(&task).unwrap().clone();
    rig.schedule();
    let (_, note, when) = rig.sent().await;
    assert_eq!(when, at(30));
    assert_eq!(note.task, Some(task.clone()));
    assert_eq!(note.from, Address::System);
    assert_eq!(note.to, Address::Owner);
    assert!(note.body.contains("crossed a budget limit"));
    assert_eq!(rig.state().await.task(&task), Some(&before));
    assert_eq!(rig.count("task.reminded"), 0);
}

// Break caught: disabling reminders also disables deadlines; event changes
// fail to rearm an existing later timer or discard its original deadline.
#[tokio::test(start_paused = true)]
async fn budget_reminders_off_rearms_for_an_earlier_task_deadline() {
    let mut rig = Rig::new().await;
    let later = deadline_task(&rig, Address::Owner, 90).await;
    budgets_only(&rig);
    rig.report(working("s1")).await; // scheduler has entered its event loop
    let earlier = deadline_task(&rig, Address::Owner, 30).await;
    let (_, first, when) = rig.sent().await;
    assert_eq!(first.task, Some(earlier));
    assert_eq!(when, at(30));
    let (_, second, when) = rig.sent().await;
    assert_eq!(second.task, Some(later));
    assert_eq!(when, at(90));
    assert_eq!(rig.count("task.reminded"), 0);
}

// Break caught: clearing missed registry observations discards budget timing.
#[tokio::test(start_paused = true)]
async fn budget_registry_lag_never_discards_task_deadlines() {
    let mut rig = Rig::new().await;
    let task = deadline_task(&rig, Address::Owner, 30).await;
    let (rows, receiver) = broadcast::channel(2);
    tokio::spawn(run(rig.org.clone(), receiver, None::<ReminderSettings>, rig.clock(), rig.cancel.clone()));
    for _ in 0..5 { rows.send(working("s1")).unwrap(); }
    let (_, note, when) = rig.sent().await;
    assert_eq!(note.task, Some(task));
    assert_eq!(when, at(30));
}

// Break caught: the earlier budget timer consumes a reminder's independent
// wait, or a reminder's message suppresses the later budget deadline.
#[tokio::test(start_paused = true)]
async fn budget_and_reminder_keep_their_independent_wakes() {
    for deadline in [30, 90] {
        let mut rig = Rig::new().await;
        let task = deadline_task(&rig, Address::Owner, deadline).await;
        rig.run(OrgCommand::Task(TaskCommand::Assign {
            task_id: task.clone(), agent_id: agent("build"),
            by: TaskActor { who: Address::Owner, lead: false },
        })).await;
        rig.schedule();
        rig.turn("s1").await;
        let (_, first, first_at) = rig.sent().await;
        let (_, second, second_at) = rig.sent().await;
        assert_eq!(first_at, at(deadline.min(DELAY)));
        assert_eq!(second_at, at(deadline.max(DELAY)));
        let (budget, reminder) = if deadline < DELAY { (first, second) } else { (second, first) };
        assert_eq!(budget.to, Address::Owner);
        assert!(budget.body.contains("crossed a budget limit"));
        assert_eq!(reminder.to, Address::Agent(agent("build")));
        assert!(reminder.body.contains("Your turn ended"));
        assert_eq!(rig.count("task.reminded"), 1);
        assert_eq!(rig.count("msg.sent"), 2);
    }
}

// Break caught: restart resends the canonical note after delivery/read/failure,
// or a state change re-arms a once-per-task budget alert.
#[tokio::test(start_paused = true)]
async fn budget_restart_retains_queued_delivered_read_and_failed_latches() {
    for outcome in ["queued", "delivered", "read", "failed"] {
        let mut rig = Rig::new().await;
        let task = deadline_task(&rig, Address::Agent(agent("lead")), -1).await;
        let scheduler = budgets_only(&rig);
        let (message_id, _, _) = rig.sent().await;
        if matches!(outcome, "delivered" | "read") {
            rig.run(OrgCommand::Delivered { message_id, session_id: sid("lead-1"), confirmation: "observed".into() }).await;
        }
        if outcome == "read" {
            rig.run(OrgCommand::ReadMessages { who: Address::Agent(agent("lead")), message_ids: vec![message_id] }).await;
        }
        if outcome == "failed" {
            rig.run(OrgCommand::DeliveryFailed { message_id, reason: "gave up".into(), final_failure: true }).await;
        }
        rig.run(OrgCommand::Task(TaskCommand::Assign {
            task_id: task, agent_id: agent("build"), by: TaskActor { who: Address::Owner, lead: false },
        })).await;
        let state = rig.state().await;
        rig.cancel.cancel();
        scheduler.await.unwrap();
        let mut after = Rig::over(state, rig.log.clone(), None).await;
        budgets_only(&after);
        let sentinel = deadline_task(&after, Address::Owner, 1).await;
        let (_, note, _) = after.sent().await;
        assert_eq!(note.task, Some(sentinel), "{outcome}: old task was already notified");
        assert_eq!(after.count("msg.sent"), 2, "{outcome}");
    }
}

// Break caught: an enqueue failure sets an in-memory latch or prevents startup
// catch-up after the organisation writer becomes available on restart.
#[tokio::test(start_paused = true)]
async fn budget_failed_append_is_recovered_by_the_next_daemon() {
    let rig = Rig::new().await;
    let task = deadline_task(&rig, Address::Owner, -1).await;
    let failing = Rig::over(rig.state().await, rig.log.clone(), Some("msg.sent")).await;
    let mut health = failing.org.health.clone();
    let scheduler = budgets_only(&failing);
    health.wait_for(Result::is_err).await.unwrap();
    assert_eq!(failing.count("msg.sent"), 0);
    failing.cancel.cancel();
    scheduler.await.unwrap();
    let state = failing.log.lock().unwrap().iter().fold(OrgState::default(), |mut state, envelope| {
        let event = OrgEvent::from_parts(&envelope.kind, &envelope.body).unwrap().unwrap();
        state.apply(envelope.seq, envelope.ts, &event).unwrap();
        state
    });
    let mut after = Rig::over(state, failing.log.clone(), None).await;
    budgets_only(&after);
    let (_, note, _) = after.sent().await;
    assert_eq!(note.task, Some(task));
    assert_eq!(after.count("msg.sent"), 1);
}

// Break caught: a committed enqueue whose reply times out is blindly resent.
// Only the command reply is held; the real actor still decides/appends/applies.
#[tokio::test(start_paused = true)]
async fn budget_lost_enqueue_reply_is_resnapshotted_before_retry() {
    let mut rig = Rig::new().await;
    let first = deadline_task(&rig, Address::Owner, -1).await;
    let (requests, mut receive) = mpsc::channel(64);
    let proxy = OrgHandle {
        requests, health: rig.org.health.clone(),
        committed: rig.org.committed.clone(), reads: None,
    };
    let actor = rig.org.clone();
    let cancel = rig.cancel.clone();
    let started = rig.started;
    let (retried, mut retries) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut held_reply = None;
        loop {
            let request = tokio::select! {
                () = cancel.cancelled() => break,
                request = receive.recv() => match request { Some(request) => request, None => break },
            };
            match request {
                super::super::Request::Snapshot(reply) => {
                    if held_reply.is_some() {
                        let _ = retried.send(t0() + TimeDelta::from_std(started.elapsed()).unwrap());
                    }
                    if let Ok(state) = actor.snapshot().await { let _ = reply.send(state); }
                },
                super::super::Request::Execute { command, now, caller_pane, reply } => {
                    let lose = held_reply.is_none() && matches!(command.as_ref(), OrgCommand::SendMessage(_));
                    let result = actor.execute_receipt(*command, now, caller_pane).await;
                    if lose { held_reply = Some(reply); } else { let _ = reply.send(result); }
                },
            }
        }
    });
    tokio::spawn(run(proxy, rig.rows.subscribe(), None::<ReminderSettings>, rig.clock(), rig.cancel.clone()));
    let (_, note, _) = rig.sent().await;
    assert_eq!(note.task, Some(first));
    // The actor-handle response budget is six virtual seconds. Retry is one
    // second after that failure, not one second after the original pass began.
    let retried_at = tokio::time::timeout(GUARD, retries.recv()).await.unwrap().unwrap();
    assert_eq!(retried_at, at(7));
    let sentinel = deadline_task(&rig, Address::Owner, 10).await;
    let (_, note, when) = rig.sent().await;
    assert_eq!(note.task, Some(sentinel), "the timed-out committed note was not resent");
    assert_eq!(when, at(10));
    assert_eq!(rig.count("msg.sent"), 2);
}

// Break caught: a later restart judges a terminal task against the current
// clock instead of when it finished, or a budget notification stops new work.
#[tokio::test(start_paused = true)]
async fn budget_early_finish_stays_quiet_and_alerted_tasks_can_keep_working() {
    let mut rig = Rig::new().await;
    let early = deadline_task(&rig, Address::Owner, 30).await;
    rig.run(OrgCommand::Task(TaskCommand::SetState {
        task_id: early, state: TaskState::Cancelled, reason: None,
        by: TaskActor { who: Address::Owner, lead: false },
    })).await;
    let due = deadline_task(&rig, Address::Owner, 60).await;
    budgets_only(&rig);
    let (_, note, when) = rig.sent().await;
    assert_eq!(note.task, Some(due.clone()));
    assert_eq!(when, at(60));
    rig.run(OrgCommand::Task(TaskCommand::Assign {
        task_id: due.clone(), agent_id: agent("build"),
        by: TaskActor { who: Address::Owner, lead: false },
    })).await;
    rig.set(&due, TaskState::Working, None, "build").await;
    assert_eq!(rig.state().await.task(&due).unwrap().state, TaskState::Working);
    assert_eq!(rig.count("msg.sent"), 1);
}
