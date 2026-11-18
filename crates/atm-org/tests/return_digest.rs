//! Synthetic report scenarios, not captured harness events.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_org::*;
mod support;
use support::decision_world::World;

fn freeze(w: &mut World, back_seq: Option<u64>) -> ReturnDigest {
    w.run(OrgCommand::Owner(OwnerCommand::CreateDigest {
        by: Address::Owner,
        back_seq,
    }));
    w.state.return_digest().unwrap().clone()
}
fn decisions(digest: &ReturnDigest, section: DigestSection) -> Vec<DecisionId> {
    digest
        .rows
        .iter()
        .filter_map(|row| match row {
            DigestRow::Decision {
                section: actual,
                decision,
                ..
            } if *actual == section => Some(decision.id),
            _ => None,
        })
        .collect()
}

// Break caught: wall-clock membership loses takes with equal/backwards timestamps.
#[test]
fn absence_membership_uses_seq_not_timestamps_and_repeat_refreshes() {
    let mut w = World::new();
    let earlier = w.request("alpha/lead", false);
    w.run(OrgCommand::Decision(DecisionCommand::TakeProvisionally(
        ProvisionalChoice {
            relayed_by: None,
            decision_id: earlier,
            option: "small".into(),
            by: "alpha/lead".parse().unwrap(),
        },
    )));
    w.away();
    let inside = w.request("beta/lead", false);
    w.take(inside);
    w.now -= chrono::Duration::hours(1);
    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    let later = w.request("gamma/chief", false);
    w.run(OrgCommand::Decision(DecisionCommand::TakeProvisionally(
        ProvisionalChoice {
            relayed_by: None,
            decision_id: later,
            option: "small".into(),
            by: "gamma/chief".parse().unwrap(),
        },
    )));
    let first = freeze(&mut w, Some(back));
    assert_eq!(decisions(&first, DigestSection::Taken), vec![inside]);
    assert_eq!(
        decisions(&first, DigestSection::Earlier),
        vec![earlier, later]
    );
    assert_eq!(first.counts.taken_for_owner, 3);
    assert_eq!(first.counts.during_absence, 1);
    w.answer(inside, "large").unwrap();
    assert!(w
        .step(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }))
        .unwrap()
        .is_empty());
    let second = freeze(&mut w, Some(back));
    assert_eq!(first.absence, second.absence);
    assert_ne!(first.snapshot_seq, second.snapshot_seq);
    assert_eq!(second.counts.overruled, 1);
    assert_eq!(first.counts.overruled, 0);
    assert_eq!(w.replay(), w.state);
}

// Break caught: project grouping/backlog/accepted weighting outranks started exposure.
#[test]
fn digest_ranks_three_projects_without_local_bias() {
    let mut w = World::new();
    let alpha = w.request("alpha/lead", true);
    let beta = w.request("beta/lead", true);
    let gamma = w.request("gamma/chief", true);
    for (d, count) in [(alpha, 3), (beta, 1), (gamma, 2)] {
        for _ in 0..count {
            let t = w.create(Some(d), None, None);
            w.assign(&t, "beta/lead").unwrap();
        }
    }
    for _ in 0..5 {
        w.create(Some(beta), None, None);
    }
    w.away();
    for d in [beta, gamma, alpha] {
        w.take(d);
    }
    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    let digest = freeze(&mut w, Some(back));
    assert_eq!(
        decisions(&digest, DigestSection::Taken),
        vec![alpha, gamma, beta]
    );
    assert_eq!(digest.counts.waiting_tasks, 5);
    assert_eq!(digest.counts.waiting_total, 5);
    assert_eq!(digest.counts.taken_for_owner, 3);
    assert_eq!(w.replay(), w.state);
}

// Break caught: row truncation, inclusive cursors, ignored UTF-8/wrapper byte budgets.
#[test]
fn pages_reassemble_without_omission_and_count_encoded_bytes() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    for _ in 0..120 {
        w.event(OrgEvent::TaskCreated(TaskCreated {
            project_id: None,
            title: "☃".repeat(2048),
            kind: TaskKind::Work,
            creator: Address::Owner,
            parent: None,
            judges: None,
            measure: TaskMeasure::Owner,
            budget: None,
            files: vec![],
            rests_on: Some(d),
        }));
    }
    w.away();
    w.take(d);
    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    let digest = freeze(&mut w, Some(back));
    assert!(serde_json::to_vec(&digest.rows).unwrap().len() > 524288);
    for limit in [2, 100] {
        let mut after = None;
        let mut rows = vec![];
        loop {
            let page = digest_page(&digest, after, limit, 524288).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= 524288);
            assert_eq!(page.counts, digest.counts);
            assert_eq!(page.total_rows, 121);
            rows.extend(page.rows);
            if page.next_after.is_none() {
                break;
            }
            after = page.next_after;
        }
        assert_eq!(rows, digest.rows);
    }
    let page = digest_page(&digest, None, 1, 524288).unwrap();
    let bytes = serde_json::to_vec(&page).unwrap().len();
    assert!(digest_page(&digest, None, 1, bytes).is_ok());
    assert!(digest_page(&digest, None, 1, bytes - 1).is_err());
    for limit in [0, 1001] {
        assert!(digest_page(&digest, None, limit, 524288).is_err());
    }
    assert!(digest_page(&digest, Some(u64::MAX), 2, 524288).is_err());
}

// Break caught: an individually oversized legacy row is silently omitted.
#[test]
fn oversized_legacy_row_is_refused_with_its_index() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    w.event(OrgEvent::TaskCreated(TaskCreated {
        project_id: None,
        title: "☃".repeat(200000),
        kind: TaskKind::Work,
        creator: Address::Owner,
        parent: None,
        judges: None,
        measure: TaskMeasure::Owner,
        budget: None,
        files: vec![],
        rests_on: Some(d),
    }));
    w.away();
    w.take(d);
    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    let digest = freeze(&mut w, Some(back));
    let page = digest_page(&digest, None, 100, 524288).unwrap();
    assert_eq!(page.next_after, Some(0));
    assert!(matches!(
        digest_page(&digest, page.next_after, 100, 524288),
        Err(DigestPageError::OversizedRow {
            row: 1,
            byte_budget: 524288
        })
    ));
}

// Break caught: current activity mutates pages or an uncommitted view becomes a report.
#[test]
fn digest_is_frozen_until_another_report_and_prefix_replay_matches() {
    let mut w = World::new();
    let first = freeze(&mut w, None);
    let d = w.request("alpha/lead", false);
    assert_eq!(w.state.return_digest(), Some(&first));
    let second = freeze(&mut w, None);
    assert_ne!(second, first);
    assert_eq!(second.counts.waiting_decisions, 1);
    assert!(
        matches!(second.rows.first(), Some(DigestRow::Waiting { decision }) if decision.id == d)
    );
    for cut in 0..=w.records.len() {
        let mut state = OrgState::default();
        for (seq, ts, event) in &w.records[..cut] {
            state.apply(*seq, *ts, event).unwrap();
        }
        for (seq, ts, event) in &w.records[cut..] {
            state.apply(*seq, *ts, event).unwrap();
        }
        assert_eq!(state, w.state);
    }
}

// Break caught: guessing a window, allowing an agent, or preparing while away.
#[test]
fn report_requires_owner_a_closed_window_and_present_switch() {
    let mut w = World::new();
    for command in [
        OwnerCommand::CreateDigest {
            by: "gamma/chief".parse().unwrap(),
            back_seq: None,
        },
        OwnerCommand::CreateDigest {
            by: Address::Owner,
            back_seq: Some(999),
        },
    ] {
        assert!(w.step(OrgCommand::Owner(command)).is_err());
    }
    w.away();
    assert!(w
        .step(OrgCommand::Owner(OwnerCommand::CreateDigest {
            by: Address::Owner,
            back_seq: None
        }))
        .is_err());
}

// Break caught: accepted work counts twice, or open backlog outranks a started task.
#[test]
fn ranking_prefers_started_then_unstarted_then_take_seq() {
    let mut w = World::new();
    let a = w.request("alpha/lead", false);
    let b = w.request("beta/lead", false);
    let c = w.request("gamma/chief", false);
    for d in [a, b] {
        let t = w.create(Some(d), None, None);
        w.assign(&t, "beta/lead").unwrap();
        if d == a {
            w.event(OrgEvent::TaskStateChanged(TaskStateChanged {
                task_id: t,
                state: TaskState::Accepted,
                reason: None,
                by: Address::Owner,
            }));
        }
    }
    w.create(Some(b), None, None);
    for _ in 0..4 {
        w.create(Some(c), None, None);
    }
    w.away();
    for d in [a, b, c] {
        w.take(d);
    }
    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    let digest = freeze(&mut w, Some(back));
    assert_eq!(decisions(&digest, DigestSection::Taken), vec![b, a, c]);
}

// Break caught: duplicate waiting tasks, guessed holds, or discarded recorded reasons.
#[test]
fn waiting_section_lists_real_holds_and_recorded_waits_once() {
    let mut w = World::new();
    let a = w.request("alpha/lead", true);
    let b = w.request("beta/lead", true);
    let waiting = w.create(None, None, None);
    w.assign(&waiting, "gamma/chief").unwrap();
    w.event(OrgEvent::TaskStateChanged(TaskStateChanged {
        task_id: waiting.clone(),
        state: TaskState::Waiting,
        reason: Some("review evidence".into()),
        by: "gamma/chief".parse().unwrap(),
    }));
    w.away();
    w.take(a);
    w.take(b);
    let root = w.create(Some(a), None, None);
    let child = w.create(Some(b), Some(root.clone()), None);
    let unrelated = w.create(None, None, None);
    let pending = w.request("gamma/chief", true);
    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    let digest = freeze(&mut w, Some(back));
    assert_eq!(
        (
            digest.counts.waiting_decisions,
            digest.counts.waiting_tasks,
            digest.counts.waiting_total
        ),
        (1, 3, 4)
    );
    let rows: Vec<_> = digest
        .rows
        .iter()
        .filter(|row| {
            matches!(
                row,
                DigestRow::Waiting { .. } | DigestRow::WaitingTask { .. }
            )
        })
        .collect();
    assert!(matches!(rows.first(),Some(DigestRow::Waiting{decision}) if decision.id==pending));
    assert!(rows.iter().any(|r| matches!(r,DigestRow::WaitingTask{task_id,reason:Some(reason),held_on:None,..} if task_id==&waiting && reason=="review evidence")));
    assert_eq!(rows.iter().filter(|r| matches!(r,DigestRow::WaitingTask{task_id,held_on:Some(id),..} if task_id==&child && *id==a)).count(),1);
    assert!(!rows
        .iter()
        .any(|r| matches!(r,DigestRow::WaitingTask{task_id,..} if task_id==&unrelated)));
}

// Break caught: statuses replace lifetime facts, confirmations inflate overrules, or null becomes zero.
#[test]
fn numbers_count_distinct_takes_and_owner_overrules() {
    let mut w = World::new();
    let a = w.request("alpha/lead", false);
    let b = w.request("beta/lead", true);
    let pending = w.request("gamma/chief", true);
    w.away();
    w.take(a);
    w.take(b);
    w.event(OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
        relayed_by: None,
        decision_id: a,
        option: "small".into(),
        by: "alpha/lead".parse().unwrap(),
    }));
    w.answer(a, "small").unwrap();
    w.answer(pending, "large").unwrap();
    w.event(OrgEvent::DecisionOverruled(
        DecisionResolution {
            decision_id: b,
            option: "large".into(),
            by: Address::Owner,
            note: None,
            standing_rule: false,
        }
        .into(),
    ));
    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    let digest = freeze(&mut w, Some(back));
    assert_eq!(
        (digest.counts.taken_for_owner, digest.counts.overruled),
        (2, 1)
    );
    assert!(digest.rows.iter().any(|r| matches!(r,DigestRow::OverruleNumber { decision_id,completed:None,required:None,cancelled:None,.. } if *decision_id==b)));
    assert_eq!(w.replay(), w.state);
}

// Break caught: skipped snapshot facts or a changed serialized complete report.
#[test]
fn golden_digest_log_replays_to_the_complete_expected_page() {
    let mut state = OrgState::default();
    for line in include_str!("fixtures/return_digest.jsonl").lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let event = OrgEvent::from_parts(value["kind"].as_str().unwrap(), &value["body"])
            .unwrap()
            .unwrap();
        state
            .apply(
                value["seq"].as_u64().unwrap(),
                value["ts"].as_str().unwrap().parse().unwrap(),
                &event,
            )
            .unwrap();
    }
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/return_digest_expected.json")).unwrap();
    assert_eq!(
        serde_json::to_value(
            digest_page(state.return_digest().unwrap(), None, 100, 524288).unwrap()
        )
        .unwrap(),
        expected
    );
}

// Break caught: the new durable report marker is silently skipped during replay.
#[test]
fn digest_created_is_a_known_additive_fact() {
    let event = OrgEvent::from_parts(
        "owner.digest_created",
        &serde_json::json!({"by":"owner","back_seq":null}),
    )
    .unwrap();
    assert!(
        event.is_some(),
        "a report must be reconstructible from its committed fact"
    );
}

fn relay(w: &mut World, decision_id: DecisionId) {
    w.step(OrgCommand::Owner(OwnerCommand::SetRelay {
        by: Address::Owner,
        agent: Some("gamma/chief".parse().unwrap()),
    }))
    .unwrap();
    w.run(OrgCommand::Decision(DecisionCommand::Relay {
        decision_id,
        option: "large".into(),
        by: "gamma/chief".parse().unwrap(),
    }));
}
fn sections_for(digest: &ReturnDigest, id: DecisionId) -> Vec<String> {
    digest
        .rows
        .iter()
        .filter_map(|row| match row {
            DigestRow::Decision {
                section, decision, ..
            } if decision.id == id => Some(
                serde_json::to_value(section)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned(),
            ),
            _ => None,
        })
        .collect()
}
fn relay_count(digest: &ReturnDigest) -> serde_json::Value {
    serde_json::to_value(&digest.counts).unwrap()["relayed"].clone()
}

// Break caught: a final answer hides a present-time relay before its first digest.
#[test]
fn present_relay_confirmed_before_digest_is_reported_once() {
    for previous in [false, true] {
        let mut w = World::new();
        if previous {
            freeze(&mut w, None);
        }
        let id = w.request("alpha/lead", true);
        relay(&mut w, id);
        w.now -= chrono::Duration::hours(1);
        w.answer(id, "large").unwrap();
        let first = freeze(&mut w, None);
        assert_eq!(sections_for(&first, id), ["relayed"]);
        assert_eq!(relay_count(&first), 1);
        assert_eq!(first.counts.taken_for_owner, 1);
        let second = freeze(&mut w, None);
        assert!(sections_for(&second, id).is_empty());
        assert_eq!(relay_count(&second), 0);
        assert_eq!(w.replay(), w.state);
    }
}

// Break caught: the overrule-number row is mistaken for coverage of the relay itself.
#[test]
fn present_relay_overruled_before_digest_is_reported_once() {
    let mut w = World::new();
    let id = w.request("alpha/lead", true);
    relay(&mut w, id);
    w.answer(id, "small").unwrap();
    let first = freeze(&mut w, None);
    assert_eq!(sections_for(&first, id), ["relayed"]);
    assert_eq!(relay_count(&first), 1);
    assert_eq!(first.counts.overruled, 1);
    let second = freeze(&mut w, None);
    assert!(sections_for(&second, id).is_empty());
    assert_eq!(second.counts.overruled, 1);
    assert_eq!(w.replay(), w.state);
}

// Break caught: a relay is duplicated in Earlier, forgotten after one digest, or revived by retry.
#[test]
fn provisional_relay_moves_to_earlier_after_first_digest() {
    let mut w = World::new();
    let old = w.request("beta/lead", false);
    w.run(OrgCommand::Decision(DecisionCommand::TakeProvisionally(
        ProvisionalChoice {
            decision_id: old,
            option: "small".into(),
            by: "beta/lead".parse().unwrap(),
            relayed_by: None,
        },
    )));
    freeze(&mut w, None);
    let id = w.request("alpha/lead", true);
    relay(&mut w, id);
    let first = freeze(&mut w, None);
    assert_eq!(sections_for(&first, id), ["relayed"]);
    assert_eq!(sections_for(&first, old), ["earlier"]);
    assert_eq!(first.counts.earlier_provisional, 1);
    assert_eq!(relay_count(&first), 1);
    assert!(w
        .step(OrgCommand::Decision(DecisionCommand::Relay {
            decision_id: id,
            option: "large".into(),
            by: "gamma/chief".parse().unwrap(),
        }))
        .unwrap()
        .is_empty());
    let second = freeze(&mut w, None);
    assert_eq!(sections_for(&second, id), ["earlier"]);
    assert_eq!(sections_for(&second, old), ["earlier"]);
    assert_eq!(second.counts.earlier_provisional, 2);
    assert_eq!(relay_count(&second), 0);
    assert_eq!(w.replay(), w.state);
}

// Break caught: a selected-absence relay gets counted again in the new section.
#[test]
fn relay_during_selected_absence_is_only_taken() {
    for confirmed in [false, true] {
        let mut w = World::new();
        w.away();
        let id = w.request("alpha/lead", true);
        relay(&mut w, id);
        let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
        if confirmed {
            w.answer(id, "large").unwrap();
        }
        for _ in 0..2 {
            let digest = freeze(&mut w, Some(back));
            assert_eq!(relay_count(&digest), 0);
            assert_eq!(sections_for(&digest, id), ["taken"]);
            assert_eq!(digest.counts.during_absence, 1);
            assert_eq!(digest.counts.earlier_provisional, 0);
        }
        assert_eq!(w.replay(), w.state);
    }
}

// Break caught: frozen seat metadata follows live designation or relays lose exposure ranking/pages.
#[test]
fn relay_section_ranking_and_page_metadata_survive_replay() {
    let mut w = World::new();
    let a = w.request("alpha/lead", true);
    let b = w.request("beta/lead", true);
    let c = w.request("gamma/chief", true);
    for id in [a, b] {
        let task = w.create(Some(id), None, None);
        w.assign(&task, "beta/lead").unwrap();
    }
    w.create(Some(b), None, None);
    for _ in 0..3 {
        w.create(Some(c), None, None);
    }
    for id in [c, a, b] {
        relay(&mut w, id);
    }
    let digest = freeze(&mut w, None);
    let initial = serde_json::to_value(digest_page(&digest, None, 100, 524288).unwrap()).unwrap();
    assert_eq!(initial["relay"], "gamma/chief");
    let ids: Vec<_> = digest
        .rows
        .iter()
        .filter_map(|row| match row {
            DigestRow::Decision { decision, .. } => Some(decision.id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, [b, a, c]);
    assert_eq!(relay_count(&digest), 3);
    w.run(OrgCommand::Owner(OwnerCommand::SetRelay {
        by: Address::Owner,
        agent: None,
    }));
    w.answer(c, "large").unwrap();
    let mut after = None;
    let mut rows = vec![];
    loop {
        let page = digest_page(w.state.return_digest().unwrap(), after, 1, 524288).unwrap();
        let encoded = serde_json::to_value(&page).unwrap();
        assert_eq!(encoded["relay"], "gamma/chief");
        assert_eq!(encoded["counts"]["relayed"], 3);
        rows.extend(page.rows);
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(rows, digest.rows);
    assert_eq!(
        serde_json::to_value(
            digest_page(w.state.return_digest().unwrap(), None, 100, 524288).unwrap()
        )
        .unwrap(),
        initial
    );
    let next = freeze(&mut w, None);
    assert_eq!(
        serde_json::to_value(digest_page(&next, None, 100, 524288).unwrap()).unwrap()["relay"],
        serde_json::Value::Null
    );
    assert_eq!(relay_count(&next), 0);
    assert_eq!(next.counts.earlier_provisional, 2);
    for cut in 0..=w.records.len() {
        let mut replay = OrgState::default();
        for (seq, at, event) in w.records.iter().take(cut).chain(w.records.iter().skip(cut)) {
            replay.apply(*seq, *at, event).unwrap();
        }
        assert_eq!(replay, w.state);
    }
}
