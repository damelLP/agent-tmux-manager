//! The away table, the stacking cap and the away settings. Pure: every input
//! is an argument.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers only
use atm_org::*;

use AwayStep::{AgentDecides, TakeProvisionally, Wait};
use AwayWait::{Irreversible as CannotBeUndone, NoRecommendation, OwnerPresent};
use DecisionClass::{ChangesIntent, Detail, Irreversible};

/// Every cell of the away table, one row each: class, recommended, away.
#[test]
fn away_table_every_cell() {
    let table = [
        // The owner is present: nothing is taken for him.
        (Detail, true, false, Wait(OwnerPresent)),
        (Detail, false, false, Wait(OwnerPresent)),
        (ChangesIntent, true, false, Wait(OwnerPresent)),
        (ChangesIntent, false, false, Wait(OwnerPresent)),
        (Irreversible, true, false, Wait(OwnerPresent)),
        (Irreversible, false, false, Wait(OwnerPresent)),
        // Away, with a recommended option.
        (Detail, true, true, AgentDecides),
        (ChangesIntent, true, true, TakeProvisionally),
        (Irreversible, true, true, Wait(CannotBeUndone)),
        // Away, nothing recommended: nothing can be recorded.
        (Detail, false, true, Wait(NoRecommendation)),
        (ChangesIntent, false, true, Wait(NoRecommendation)),
        (Irreversible, false, true, Wait(CannotBeUndone)),
    ];
    for (class, recommended, away, expected) in table {
        assert_eq!(
            away_step(class, recommended, away),
            expected,
            "{class:?}, recommended {recommended}, away {away}"
        );
    }
}

const STATUSES: [DecisionStatus; 6] = [
    DecisionStatus::Pending,
    DecisionStatus::TakenProvisionally,
    DecisionStatus::Resolved,
    DecisionStatus::Overruled,
    DecisionStatus::Expired,
    DecisionStatus::Withdrawn,
];

#[test]
fn default_cap_holds_every_further_task_on_a_provisional_change_of_intent() {
    assert_eq!(DEFAULT_STACKING_CAP, 1);
    for started in [0, 1, 7] {
        assert_eq!(
            stacking_step(
                DecisionStatus::TakenProvisionally,
                ChangesIntent,
                started,
                DEFAULT_STACKING_CAP
            ),
            StackingStep::Hold,
            "{started} started"
        );
    }
}

#[test]
fn only_a_provisional_change_of_intent_holds() {
    for status in STATUSES {
        for class in [Detail, ChangesIntent, Irreversible] {
            let expected = if status == DecisionStatus::TakenProvisionally && class == ChangesIntent
            {
                StackingStep::Hold
            } else {
                StackingStep::Start
            };
            assert_eq!(
                stacking_step(status, class, 0, 1),
                expected,
                "{status:?} {class:?}"
            );
        }
    }
}

/// The task in progress is the first of the cap: N allows N - 1 further tasks.
#[test]
fn cap_counts_the_task_in_progress_as_the_first() {
    let step = |started, cap| {
        stacking_step(
            DecisionStatus::TakenProvisionally,
            ChangesIntent,
            started,
            cap,
        )
    };
    for (started, cap, expected) in [
        (0, 1, StackingStep::Hold),
        (0, 2, StackingStep::Start),
        (1, 2, StackingStep::Hold),
        (1, 3, StackingStep::Start),
        (2, 3, StackingStep::Hold),
        (5, 3, StackingStep::Hold),
        // A cap of zero cannot be set; it would hold like one.
        (0, 0, StackingStep::Hold),
        (0, u32::MAX, StackingStep::Start),
        (usize::MAX, u32::MAX, StackingStep::Hold),
    ] {
        assert_eq!(step(started, cap), expected, "{started} started, cap {cap}");
    }
}

#[test]
fn away_settings_default_to_one_and_read_the_cap() {
    assert_eq!(AwaySettings::default().stacking_cap, 1);
    for text in ["", "\n", "# only a comment\n"] {
        assert_eq!(
            parse_away_settings(text).unwrap(),
            AwaySettings::default(),
            "{text:?}"
        );
    }
    assert_eq!(
        parse_away_settings("stacking_cap = 1\n")
            .unwrap()
            .stacking_cap,
        1
    );
    assert_eq!(
        parse_away_settings("stacking_cap = 3")
            .unwrap()
            .stacking_cap,
        3
    );
    assert_eq!(
        parse_away_settings("stacking_cap = 4294967295")
            .unwrap()
            .stacking_cap,
        u32::MAX
    );
}

#[test]
fn away_settings_refuse_a_cap_below_one_unknown_keys_and_wrong_types() {
    assert!(matches!(
        parse_away_settings("stacking_cap = 0"),
        Err(AwaySettingsError::CapBelowOne)
    ));
    assert_eq!(
        parse_away_settings("stacking_cap = 0")
            .unwrap_err()
            .to_string(),
        "stacking_cap must be at least 1"
    );
    for text in [
        "stacking_cap = -1",
        "stacking_cap = 4294967296",
        "stacking_cap = \"1\"",
        "stacking_cap = 1.0",
        "stacking_cap = true",
        "stacking_cpa = 2",
        "stacking_cap = 2\nfuture = 1",
        "[stacking_cap]\n",
        "not toml",
    ] {
        assert!(
            matches!(parse_away_settings(text), Err(AwaySettingsError::Toml(_))),
            "{text:?}"
        );
    }
}
